//! Compilation + rendering orchestrator. Port of
//! `django.template.base.Template`. source -> Lexer -> Parser ->
//! NodeList -> render(Context).

use std::collections::HashMap;

use pyo3::prelude::*;

use crate::context::{Context, ContextDict, Value};
use crate::errors::TemplateError;
use crate::lexer::{DebugLexer, Lexer};
use crate::nodes::NodeList;
use crate::parser::Parser;
use crate::py_bindings;

/// Mirrors `django.template.base.Template`.
#[derive(Debug)]
pub struct Template {
    pub name: Option<String>,
    pub source: String,
    pub nodelist: NodeList,
    pub debug: bool,
    /// Output when a variable lookup fails (`Engine.string_if_invalid`).
    pub string_if_invalid: String,
}

impl Template {
    pub fn new(
        template_string: impl Into<String>,
        name: Option<String>,
        debug: bool,
        string_if_invalid: Option<String>,
    ) -> Result<Self, TemplateError> {
        Self::new_with_engine(template_string, name, debug, string_if_invalid, None)
    }

    /// Threads a Python `Engine` into the parser so its
    /// `template_builtins` and `template_libraries` register before
    /// parsing. Without this, third-party builtin tags (cotton,
    /// debug_toolbar) don't parse.
    pub fn new_with_engine<'py>(
        template_string: impl Into<String>,
        name: Option<String>,
        debug: bool,
        string_if_invalid: Option<String>,
        engine: Option<&pyo3::Bound<'py, pyo3::PyAny>>,
    ) -> Result<Self, TemplateError> {
        let source: String = template_string.into();
        let nodelist = Self::compile_nodelist_with_engine(&source, name.as_deref(), debug, engine)?;

        Ok(Self {
            name,
            source,
            nodelist,
            debug,
            string_if_invalid: string_if_invalid.unwrap_or_default(),
        })
    }

    /// Mirrors `Template.compile_nodelist`.
    pub fn compile_nodelist(source: &str, debug: bool) -> Result<NodeList, TemplateError> {
        Self::compile_nodelist_with_engine(source, None, debug, None)
    }

    /// `compile_nodelist` + engine builtins/libraries registration.
    /// `name` becomes the Origin (defaults to `UNKNOWN_SOURCE`).
    pub fn compile_nodelist_with_engine<'py>(
        source: &str,
        name: Option<&str>,
        debug: bool,
        engine: Option<&pyo3::Bound<'py, pyo3::PyAny>>,
    ) -> Result<NodeList, TemplateError> {
        let _g_compile = crate::prof::Guard::new("compile_nodelist:total");
        // With an engine: tokenise via Django's Lexer/DebugLexer so we
        // pick up monkey-patches (django-cotton's nested_tag_support
        // is the canonical case). Without one: pure-Rust lexer.
        let tokens = {
            let _g = crate::prof::Guard::new("compile_nodelist:tokenize");
            match engine {
                Some(engine_obj) => Python::attach(|py| -> Result<Vec<crate::lexer::Token>, TemplateError> {
                    // Always use Django's lexer when an engine is
                    // present. Identity-checking against a snapshot is
                    // unsafe because monkey-patches usually land before
                    // our AppConfig.ready runs.
                    py_tokenize_via_django(py, source, debug, engine_obj)
                })?,
                None => {
                    if debug {
                        let mut lexer = DebugLexer::new(source);
                        lexer.tokenize()
                    } else {
                        let mut lexer = Lexer::new(source);
                        lexer.tokenize()
                    }
                }
            }
        };

        let _g_setup = crate::prof::Guard::new("compile_nodelist:setup");
        let mut parser = Parser::new(tokens);
        // Default to `Origin(UNKNOWN_SOURCE)` (base.py:151-152).
        let origin_name = name.unwrap_or(crate::nodes::UNKNOWN_SOURCE);
        let mut origin = crate::nodes::Origin::new(origin_name);
        origin.template_name = Some(origin.name.clone());
        parser.origin = Some(origin);
        crate::tags::register_default_tags(&mut parser);

        Python::attach(|py| -> Result<(), TemplateError> {
            py_bindings::register_default_filters(py, &mut parser);

            // engine.template_builtins is a list[Library]; register
            // each so `{% cotton %}` etc. are visible during parsing.
            if let Some(engine_obj) = engine {
                if let Ok(builtins) = engine_obj.getattr(pyo3::intern!(py, "template_builtins")) {
                    if let Ok(iter) = builtins.try_iter() {
                        for lib_result in iter {
                            if let Ok(lib) = lib_result {
                                // Best-effort: skip libraries that raise.
                                let _ = parser.add_python_library(py, &lib);
                            }
                        }
                    }
                }

                // template_libraries: alias -> Library for {% load name %}
                // resolution (the standard stock Django path).
                if let Ok(libs) = engine_obj.getattr(pyo3::intern!(py, "template_libraries")) {
                    if let Ok(libs_dict) = libs.cast::<pyo3::types::PyDict>() {
                        for (name, lib) in libs_dict.iter() {
                            if let Ok(name_str) = name.extract::<String>() {
                                parser.libraries.insert(name_str, lib.unbind());
                            }
                        }
                    }
                }
            }

            Ok(())
        })?;
        drop(_g_setup);

        let _g_parse = crate::prof::Guard::new("compile_nodelist:parse");
        parser.parse(&[])
    }

    /// Mirrors `Template.render(context)` minus `push_state` /
    /// `bind_template` (added when full inheritance lands).
    pub fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let _g = crate::prof::Guard::new("Template::render");
        if !self.string_if_invalid.is_empty() && context.string_if_invalid.is_empty() {
            context.string_if_invalid = self.string_if_invalid.clone();
        }
        // BlockContext is on context.block_context; nested renders get
        // a fresh Context via PyTemplate::render so no save/restore.
        let safe = self.nodelist.render(py, context)?;
        Ok(safe.as_str().to_owned())
    }
}

/// Tokenise via Django's (possibly monkey-patched) Lexer/DebugLexer.
/// Django's TokenType ints (TEXT=0..COMMENT=3) match ours. `position`
/// is `(start, end)` on DebugLexer; we keep only `start`.
fn py_tokenize_via_django(
    py: pyo3::Python<'_>,
    source: &str,
    debug: bool,
    _engine: &pyo3::Bound<'_, pyo3::PyAny>,
) -> Result<Vec<crate::lexer::Token>, TemplateError> {
    use pyo3::intern;
    use pyo3::types::{PyAnyMethods, PyList};

    let base = py
        .import("django.template.base")
        .map_err(|e| TemplateError::Internal(format!("import django.template.base: {e}")))?;

    let lexer_cls_name = if debug { "DebugLexer" } else { "Lexer" };
    let lexer_cls = base.getattr(lexer_cls_name).map_err(|e| {
        TemplateError::Internal(format!("django.template.base.{lexer_cls_name}: {e}"))
    })?;
    let lexer_obj = lexer_cls
        .call1((source,))
        .map_err(|e| TemplateError::Internal(format!("{lexer_cls_name}(source): {e}")))?;
    let py_tokens = lexer_obj
        .call_method0(intern!(py, "tokenize"))
        .map_err(|e| TemplateError::Internal(format!("{lexer_cls_name}.tokenize: {e}")))?;

    // Interned attr names to skip per-getattr PyString allocations.
    let attr_token_type = intern!(py, "token_type");
    let attr_value = intern!(py, "value");
    let attr_contents = intern!(py, "contents");
    let attr_lineno = intern!(py, "lineno");
    let attr_position = intern!(py, "position");

    // Fast list path; fall back to iterator (monkey-patches may
    // return generators).
    let mut out: Vec<crate::lexer::Token> = if let Ok(list) = py_tokens.cast::<PyList>() {
        let n = list.len();
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: i is strictly in [0, n) and we hold the GIL.
            let tok = unsafe { list.get_item_unchecked(i) };
            v.push(extract_token(
                py,
                &tok,
                attr_token_type,
                attr_value,
                attr_contents,
                attr_lineno,
                attr_position,
            )?);
        }
        v
    } else {
        let py_token_iter = py_tokens
            .try_iter()
            .map_err(|e| TemplateError::Internal(format!("tokens not iterable: {e}")))?;
        let mut v = Vec::new();
        for tok_result in py_token_iter {
            let tok = tok_result
                .map_err(|e| TemplateError::Internal(format!("iterate tokens: {e}")))?;
            v.push(extract_token(
                py,
                &tok,
                attr_token_type,
                attr_value,
                attr_contents,
                attr_lineno,
                attr_position,
            )?);
        }
        v
    };

    out.shrink_to_fit();
    Ok(out)
}

/// Interned attr-name args; no PyString allocated per call.
#[inline]
fn extract_token(
    _py: pyo3::Python<'_>,
    tok: &pyo3::Bound<'_, pyo3::PyAny>,
    attr_token_type: &pyo3::Bound<'_, pyo3::types::PyString>,
    attr_value: &pyo3::Bound<'_, pyo3::types::PyString>,
    attr_contents: &pyo3::Bound<'_, pyo3::types::PyString>,
    attr_lineno: &pyo3::Bound<'_, pyo3::types::PyString>,
    attr_position: &pyo3::Bound<'_, pyo3::types::PyString>,
) -> Result<crate::lexer::Token, TemplateError> {
    use pyo3::types::PyAnyMethods;

    // Django uses IntEnum; `.value` gives the int. Some monkey-patches
    // set token_type to a raw int.
    let tt_obj = tok
        .getattr(attr_token_type)
        .map_err(|e| TemplateError::Internal(format!("token.token_type: {e}")))?;
    let tt_int: u8 = match tt_obj.getattr(attr_value) {
        Ok(v) => v
            .extract::<u8>()
            .map_err(|e| TemplateError::Internal(format!("token_type.value: {e}")))?,
        Err(_) => tt_obj
            .extract::<u8>()
            .map_err(|e| TemplateError::Internal(format!("token_type: {e}")))?,
    };
    let token_type = match tt_int {
        0 => crate::lexer::TokenType::Text,
        1 => crate::lexer::TokenType::Var,
        2 => crate::lexer::TokenType::Block,
        3 => crate::lexer::TokenType::Comment,
        other => {
            return Err(TemplateError::Internal(format!(
                "unknown Django TokenType value: {other}"
            )));
        }
    };

    let contents: String = tok
        .getattr(attr_contents)
        .and_then(|c| c.extract::<String>())
        .map_err(|e| TemplateError::Internal(format!("token.contents: {e}")))?;

    let lineno: usize = tok
        .getattr(attr_lineno)
        .and_then(|l| l.extract::<usize>())
        .map_err(|e| TemplateError::Internal(format!("token.lineno: {e}")))?;

    // `(start, end)` on DebugLexer, `None` on Lexer.
    let (position, source_len): (Option<usize>, Option<usize>) = match tok.getattr(attr_position) {
        Ok(p) if p.is_none() => (None, None),
        Ok(p) => match p.extract::<(usize, usize)>() {
            Ok((start, end)) => (Some(start), Some(end - start)),
            Err(_) => (None, None),
        },
        Err(_) => (None, None),
    };

    let mut token = crate::lexer::Token::new(
        token_type, contents, position, lineno,
    );
    if let Some(sl) = source_len {
        token = token.with_source_len(sl);
    }
    Ok(token)
}

pub fn render_to_string(
    template_string: &str,
    variables: HashMap<String, Value>,
) -> Result<String, TemplateError> {
    let template = Template::new(template_string, None, false, None)?;
    let context_dict: ContextDict = variables;
    let mut context = Context::new(Some(context_dict));
    Python::attach(|py| template.render(py, &mut context))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn test_compile_text_only() {
        let t = Template::new("Hello world", None, false, None).unwrap();
        assert_eq!(t.source, "Hello world");
        assert_eq!(t.nodelist.len(), 1);
        assert!(!t.debug);
        assert_eq!(t.string_if_invalid, "");
    }

    #[test]
    fn test_compile_with_variable() {
        let t = Template::new("Hello {{ name }}!", None, false, None).unwrap();
        assert_eq!(t.nodelist.len(), 3);
    }

    #[test]
    fn test_compile_debug_mode() {
        let t = Template::new("{{ x }}", None, true, None).unwrap();
        assert!(t.debug);
        assert_eq!(t.nodelist.len(), 1);
    }

    #[test]
    fn test_compile_with_name() {
        let t = Template::new("text", Some("my_template.html".into()), false, None).unwrap();
        assert_eq!(t.name.as_deref(), Some("my_template.html"));
    }

    #[test]
    fn test_compile_with_string_if_invalid() {
        let t = Template::new("{{ x }}", None, false, Some("[INVALID]".into())).unwrap();
        assert_eq!(t.string_if_invalid, "[INVALID]");
    }

    #[test]
    fn test_compile_comment_stripped() {
        let t = Template::new("before{# comment #}after", None, false, None).unwrap();
        assert_eq!(t.nodelist.len(), 2);
    }

    #[test]
    fn test_compile_unknown_block_tag_error() {
        let err = Template::new("{% unknowntag x %}", None, false, None).unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Invalid block tag"), "unexpected: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_empty_var_error() {
        let err = Template::new("{{  }}", None, false, None).unwrap_err();
        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Empty variable tag"), "unexpected: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }


    #[test]
    fn test_render_text_only() {
        Python::attach(|py| {
            let t = Template::new("Hello world", None, false, None).unwrap();
            let mut ctx = Context::new(None);
            assert_eq!(t.render(py, &mut ctx).unwrap(), "Hello world");
        });
    }

    #[test]
    fn test_render_with_variable() {
        Python::attach(|py| {
            let t = Template::new("Hello, {{ name }}!", None, false, None).unwrap();
            let mut vars = HashMap::new();
            vars.insert("name".to_owned(), Value::String("Alice".to_owned()));
            let mut ctx = Context::new(Some(vars));
            assert_eq!(t.render(py, &mut ctx).unwrap(), "Hello, Alice!");
        });
    }

    #[test]
    fn test_render_missing_variable() {
        Python::attach(|py| {
            let t = Template::new("Hello, {{ name }}!", None, false, None).unwrap();
            let mut ctx = Context::new(None);
            assert_eq!(t.render(py, &mut ctx).unwrap(), "Hello, !");
        });
    }

    #[test]
    fn test_render_multiple_variables() {
        Python::attach(|py| {
            let t = Template::new(
                "{{ greeting }}, {{ name }}!",
                None,
                false,
                None,
            )
            .unwrap();
            let mut vars = HashMap::new();
            vars.insert("greeting".to_owned(), Value::String("Hi".to_owned()));
            vars.insert("name".to_owned(), Value::String("Bob".to_owned()));
            let mut ctx = Context::new(Some(vars));
            assert_eq!(t.render(py, &mut ctx).unwrap(), "Hi, Bob!");
        });
    }

    #[test]
    fn test_render_integer_value() {
        Python::attach(|py| {
            let t = Template::new("Count: {{ n }}", None, false, None).unwrap();
            let mut vars = HashMap::new();
            vars.insert("n".to_owned(), Value::Int(42));
            let mut ctx = Context::new(Some(vars));
            assert_eq!(t.render(py, &mut ctx).unwrap(), "Count: 42");
        });
    }

    #[test]
    fn test_render_bool_value() {
        Python::attach(|py| {
            let t = Template::new("Active: {{ flag }}", None, false, None).unwrap();
            let mut vars = HashMap::new();
            vars.insert("flag".to_owned(), Value::Bool(true));
            let mut ctx = Context::new(Some(vars));
            assert_eq!(t.render(py, &mut ctx).unwrap(), "Active: True");
        });
    }


    #[test]
    fn test_render_to_string_basic() {
        let mut vars = HashMap::new();
        vars.insert("name".to_owned(), Value::from("World"));
        let output = render_to_string("Hello, {{ name }}!", vars).unwrap();
        assert_eq!(output, "Hello, World!");
    }

    #[test]
    fn test_render_to_string_empty_context() {
        let output = render_to_string("Static text", HashMap::new()).unwrap();
        assert_eq!(output, "Static text");
    }

    #[test]
    fn test_render_to_string_with_comment() {
        let output =
            render_to_string("A{# hidden #}B", HashMap::new()).unwrap();
        assert_eq!(output, "AB");
    }
}
