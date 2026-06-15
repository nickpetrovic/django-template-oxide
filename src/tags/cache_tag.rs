//! `{% cache timeout fragment_name vary_on... %}`. Port of
//! `defaulttags.do_cache`. Renders via `django.core.cache`.

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
        // Resolve timeout. Django raises TemplateSyntaxError if the variable
        // doesn't exist or isn't an integer.
        let timeout_result =
            crate::nodes::resolve_expression_rust(py, &self.expire_time_expr, context);
        let timeout_val = match timeout_result {
            Ok(v) => v,
            Err(TemplateError::VariableDoesNotExist { .. }) | Err(TemplateError::Internal(_)) => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "\"cache\" tag got an unknown variable: {:?}",
                    self.expire_time_expr
                )));
            }
            Err(e) => return Err(e),
        };

        // Check for empty string_if_invalid result (variable not found)
        if let Value::String(ref s) = timeout_val
            && s.is_empty()
        {
            return Err(TemplateError::TemplateSyntaxError(format!(
                "\"cache\" tag got an unknown variable: {:?}",
                self.expire_time_expr
            )));
        }

        let timeout: Option<i64> = match &timeout_val {
            Value::Int(n) => Some(*n),
            Value::Float(f) => Some(*f as i64),
            Value::None => None,
            Value::String(s) => match s.parse::<i64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "\"cache\" tag got a non-integer timeout value: {:?}",
                        s
                    )));
                }
            },
            Value::SafeString(s) => match s.parse::<i64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "\"cache\" tag got a non-integer timeout value: {:?}",
                        s.as_ref()
                    )));
                }
            },
            _ => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "\"cache\" tag got a non-integer timeout value: {:?}",
                    timeout_val.to_string()
                )));
            }
        };

        // fragment_name is a raw string in Django (not compiled as filter)
        let fragment_name = &self.fragment_name_str;

        let vary_on: Vec<String> = self
            .vary_on
            .iter()
            .map(|fe| {
                let val = resolve_if_value(py, fe, context);
                val.to_string()
            })
            .collect();

        let cache_alias: Option<String> = match &self.cache_alias {
            Some(fe) => {
                let val = resolve_if_value(py, fe, context);
                Some(val.to_string())
            }
            None => None,
        };

        let result: Result<String, TemplateError> = (|| {
            let cache_utils = py.import("django.templatetags.cache").map_err(|e| {
                TemplateError::Internal(format!("Cannot import django.templatetags.cache: {e}"))
            })?;
            let make_key = cache_utils
                .getattr("make_template_fragment_key")
                .map_err(|e| {
                    TemplateError::Internal(format!("Cannot get make_template_fragment_key: {e}"))
                })?;
            let py_vary: Vec<&str> = vary_on.iter().map(|s| s.as_str()).collect();
            let cache_key = make_key
                .call1((fragment_name.as_str(), py_vary))
                .map_err(|e| {
                    TemplateError::Internal(format!("make_template_fragment_key failed: {e}"))
                })?
                .extract::<String>()
                .map_err(|e| TemplateError::Internal(format!("cache key not a string: {e}")))?;

            let caches = py
                .import("django.core.cache")
                .map_err(|e| {
                    TemplateError::Internal(format!("Cannot import django.core.cache: {e}"))
                })?
                .getattr("caches")
                .map_err(|e| TemplateError::Internal(format!("Cannot get caches: {e}")))?;

            // Determine which cache to use:
            // 1. If cache_alias is explicitly provided, use it
            // 2. Otherwise try "template_fragments", fall back to "default"
            let cache = if let Some(ref alias) = cache_alias {
                caches.get_item(alias.as_str()).map_err(|_| {
                    TemplateError::TemplateSyntaxError(format!(
                        "Invalid cache name specified for cache tag: {:?}",
                        alias
                    ))
                })?
            } else {
                match caches.get_item("template_fragments") {
                    Ok(c) => c,
                    Err(_) => caches.get_item("default").map_err(|e| {
                        TemplateError::Internal(format!("Cannot get default cache: {e}"))
                    })?,
                }
            };

            let cached = cache
                .call_method1("get", (cache_key.as_str(),))
                .map_err(|e| TemplateError::Internal(format!("cache.get() failed: {e}")))?;

            if !cached.is_none() {
                return cached.extract::<String>().map_err(|e| {
                    TemplateError::Internal(format!("cached value not a string: {e}"))
                });
            }

            // Miss: render and cache.
            let safe = self.nodelist.render(py, context)?;
            let rendered = safe.as_str().to_owned();

            if let Some(t) = timeout {
                cache
                    .call_method("set", (cache_key.as_str(), rendered.as_str(), t), None)
                    .map_err(|e| TemplateError::Internal(format!("cache.set() failed: {e}")))?;
            } else {
                cache
                    .call_method1("set", (cache_key.as_str(), rendered.as_str()))
                    .map_err(|e| TemplateError::Internal(format!("cache.set() failed: {e}")))?;
            }

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

/// {% cache timeout fragment_name [vary_on ...] [using="alias"] %}
pub fn compile_cache(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let nodelist = parser.parse(&["endcache"])?;
    parser.delete_first_token();

    let bits = token.split_contents();
    if bits.len() < 3 {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{:?}' tag requires at least 2 arguments.",
            bits.first().unwrap_or(&String::new())
        )));
    }

    // Check if last token starts with "using=" (Django's syntax)
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
    // fragment_name is a raw string, not compiled as a filter expression
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
