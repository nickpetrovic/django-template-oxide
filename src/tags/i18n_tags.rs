//! i18n tags. Port of `django.templatetags.i18n`: `{% translate %}`,
//! `{% blocktranslate %}`, `{% language %}`, `{% get_*_language* %}`.

use std::collections::HashMap;

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::{Parser, TagCompileFn};
use crate::variable::FilterExpression;

#[allow(unused_imports)]
use crate::impl_node_metadata;

fn resolve_expr(py: Python<'_>, fe: &FilterExpression, context: &mut Context) -> Value {
    crate::nodes::resolve_expression_rust(py, fe, context)
        .unwrap_or_else(|_| Value::String(String::new()))
}

// {% translate %} / {% trans %}

#[derive(Debug)]
struct TranslateNode {
    message: FilterExpression,
    /// `pgettext` context.
    message_context: Option<FilterExpression>,
    /// Mark for extraction only; don't translate.
    noop: bool,
    asvar: Option<String>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for TranslateNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let mut fe = self.message.clone();

        // `filter_expression.var.translate = not self.noop`.
        match &mut fe.var {
            crate::variable::FilterExpressionVar::Var(var) => {
                var.translate = !self.noop;
                if let Some(ref ctx_expr) = self.message_context {
                    let ctx_val = resolve_expr(py, ctx_expr, context);
                    var.message_context = Some(ctx_val.to_string());
                }
            }
            crate::variable::FilterExpressionVar::Constant(constant) => {
                // Constants resolved at parse time; translate then
                // re-apply filters.
                if !self.noop
                    && let Some(msg) = constant.clone()
                {
                    let translation = py.import("django.utils.translation").map_err(|e| {
                        TemplateError::Internal(format!("Cannot import translation: {e}"))
                    })?;

                    // Django doubles percent signs before calling gettext
                    // because PO files store msgids with %% for literal %.
                    // See Variable.resolve: `msgid = value.replace("%", "%%")`
                    let msgid = msg.replace('%', "%%");

                    let translated = if let Some(ctx_expr) = &self.message_context {
                        let ctx_val = resolve_expr(py, ctx_expr, context);
                        let ctx_str = ctx_val.to_string();
                        translation
                            .call_method1("pgettext", (ctx_str.as_str(), msgid.as_str()))
                            .and_then(|r| r.extract::<String>())
                            .unwrap_or_else(|_| msg.clone())
                    } else {
                        translation
                            .call_method1("gettext", (msgid.as_str(),))
                            .and_then(|r| r.extract::<String>())
                            .unwrap_or_else(|_| msg.clone())
                    };

                    fe.var = crate::variable::FilterExpressionVar::Constant(Some(translated));
                }
            }
        }

        let mut output = crate::nodes::resolve_expression_rust(py, &fe, context)
            .unwrap_or_else(|_| Value::String(String::new()));

        // Translated constants need autoescape (user-facing text);
        // Variable inputs keep their existing safe status.
        let was_constant = matches!(
            self.message.var,
            crate::variable::FilterExpressionVar::Constant(_)
        );
        if !self.noop
            && was_constant
            && let Value::SafeString(s) = output
        {
            output = Value::String(s.to_string());
        }

        let mut value = crate::nodes::render_value_in_context(&output, context);

        // Unescape Django's source-level `%%`.
        value = value.replace("%%", "%");

        if let Some(ref asvar) = self.asvar {
            // Already escaped; flag SafeString.
            context.set(asvar.clone(), Value::SafeString(value.into()));
            Ok(String::new())
        } else {
            Ok(value)
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_translate(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'translate' tag requires at least one argument.".into(),
        ));
    }

    let message = parser.compile_filter(&bits[1])?;
    let mut noop = false;
    let mut asvar = None;
    let mut message_context = None;

    let mut i = 2;
    while i < bits.len() {
        match bits[i].as_str() {
            "noop" => {
                noop = true;
                i += 1;
            }
            "as" => {
                if i + 1 >= bits.len() {
                    return Err(TemplateError::TemplateSyntaxError(
                        "'translate' tag with 'as' requires a variable name.".into(),
                    ));
                }
                asvar = Some(bits[i + 1].clone());
                i += 2;
            }
            "context" => {
                if i + 1 >= bits.len() {
                    return Err(TemplateError::TemplateSyntaxError(
                        "'translate' tag with 'context' requires a value.".into(),
                    ));
                }
                message_context = Some(parser.compile_filter(&bits[i + 1])?);
                i += 2;
            }
            other => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Unknown argument for 'translate' tag: '{}'.",
                    other,
                )));
            }
        }
    }

    Ok(Box::new(TranslateNode {
        message,
        message_context,
        noop,
        asvar,
        token_field: None,
        origin_field: None,
    }))
}

// {% blocktranslate %} / {% blocktrans %}

#[derive(Debug)]
struct BlockTranslateNode {
    /// `with var=expr` bindings.
    extra_context: Vec<(String, FilterExpression)>,
    singular: NodeList,
    plural: Option<NodeList>,
    countervar: Option<String>,
    counter: Option<FilterExpression>,
    message_context: Option<FilterExpression>,
    /// Strip leading/trailing whitespace per line.
    trimmed: bool,
    asvar: Option<String>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for BlockTranslateNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let mut extra_values: HashMap<String, Value> = HashMap::new();
        for (name, fe) in &self.extra_context {
            let val = resolve_expr(py, fe, context);
            extra_values.insert(name.clone(), val);
        }

        if let Some(ref countervar) = self.countervar
            && let Some(ref counter_expr) = self.counter
        {
            let count_val = resolve_expr(py, counter_expr, context);
            extra_values.insert(countervar.clone(), count_val);
        }

        context.push_with(extra_values);

        // Counter must be numeric (Django raises TemplateSyntaxError).
        if let Some(ref countervar) = self.countervar
            && let Some(val) = context.get(countervar)
        {
            match val {
                Value::Int(_) | Value::Float(_) => {}
                _ => {
                    context.pop();
                    let tag_name = self
                        .token_field
                        .as_ref()
                        .and_then(|t| t.contents.split_whitespace().next())
                        .unwrap_or("blocktranslate");
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' argument to '{}' tag must be a number.",
                        countervar, tag_name,
                    )));
                }
            }
        }

        // Build msgid with `%(name)s` placeholders, matching Django's
        // BlockTranslateNode.render_token_list.
        let (mut msgid, singular_vars) = render_token_list(&self.singular);

        if self.trimmed {
            msgid = trim_message(&msgid);
        }

        let translation = py
            .import("django.utils.translation")
            .map_err(|e| TemplateError::Internal(format!("Cannot import translation: {e}")))?;

        let translated = if let Some(ref plural_nl) = self.plural {
            let (mut plural_msg, _) = render_token_list(plural_nl);
            if self.trimmed {
                plural_msg = trim_message(&plural_msg);
            }

            let count = if let Some(ref countervar) = self.countervar {
                match context.get(countervar) {
                    Some(Value::Int(n)) => *n,
                    Some(Value::Float(f)) => *f as i64,
                    _ => 1,
                }
            } else {
                1
            };

            if let Some(ref ctx_expr) = self.message_context {
                let ctx_val = resolve_expr(py, ctx_expr, context);
                let ctx_str = ctx_val.to_string();
                let npgettext = translation
                    .getattr("npgettext")
                    .map_err(|e| TemplateError::Internal(format!("Cannot get npgettext: {e}")))?;
                let result = npgettext
                    .call1((ctx_str.as_str(), msgid.as_str(), plural_msg.as_str(), count))
                    .map_err(|e| TemplateError::Internal(format!("npgettext failed: {e}")))?;
                result.extract::<String>().unwrap_or_else(|_| msgid.clone())
            } else {
                let ngettext = translation
                    .getattr("ngettext")
                    .map_err(|e| TemplateError::Internal(format!("Cannot get ngettext: {e}")))?;
                let result = ngettext
                    .call1((msgid.as_str(), plural_msg.as_str(), count))
                    .map_err(|e| TemplateError::Internal(format!("ngettext failed: {e}")))?;
                result.extract::<String>().unwrap_or_else(|_| msgid.clone())
            }
        } else {
            if let Some(ref ctx_expr) = self.message_context {
                let ctx_val = resolve_expr(py, ctx_expr, context);
                let ctx_str = ctx_val.to_string();
                let pgettext = translation
                    .getattr("pgettext")
                    .map_err(|e| TemplateError::Internal(format!("Cannot get pgettext: {e}")))?;
                let result = pgettext
                    .call1((ctx_str.as_str(), msgid.as_str()))
                    .map_err(|e| TemplateError::Internal(format!("pgettext failed: {e}")))?;
                result.extract::<String>().unwrap_or_else(|_| msgid.clone())
            } else {
                let gettext = translation
                    .getattr("gettext")
                    .map_err(|e| TemplateError::Internal(format!("Cannot get gettext: {e}")))?;
                let result = gettext
                    .call1((msgid.as_str(),))
                    .map_err(|e| TemplateError::Internal(format!("gettext failed: {e}")))?;
                result.extract::<String>().unwrap_or_else(|_| msgid.clone())
            }
        };

        // Data dict for `%(name)s`; mirrors Django's `render_value`.
        // Missing vars use string_if_invalid.
        let mut all_vars = singular_vars;
        if let Some(ref plural_nl) = self.plural {
            let (_, plural_vars) = render_token_list(plural_nl);
            for v in plural_vars {
                if !all_vars.contains(&v) {
                    all_vars.push(v);
                }
            }
        }

        let mut data: HashMap<String, String> = HashMap::new();
        for var_name in &all_vars {
            let val = match context.get(var_name) {
                Some(v) => crate::nodes::render_value_in_context(&v.clone(), context),
                None => context.string_if_invalid.clone(),
            };
            data.insert(var_name.clone(), val);
        }

        // Mirrors Django's `result %= data`.
        let result = interpolate_message_with_data(&translated, &data, context);

        context.pop();

        if let Some(ref asvar) = self.asvar {
            context.set(asvar.clone(), Value::SafeString(result.into()));
            Ok(String::new())
        } else {
            Ok(result)
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["singular", "plural"]
    }
}

/// Build `(msgid, vars)` with `%(name)s` placeholders, matching
/// `BlockTranslateNode.render_token_list`.
fn render_token_list(nodelist: &NodeList) -> (String, Vec<String>) {
    use crate::nodes::NodeEntry;

    let mut result = String::new();
    let mut vars = Vec::new();

    for entry in nodelist.iter_entries() {
        match entry {
            NodeEntry::Text(s) => {
                // `token.token_string.replace('%', '%%')` per Django.
                result.push_str(&s.replace('%', "%%"));
            }
            NodeEntry::Variable(var_node) => {
                if let Some(token) = var_node.token() {
                    let var_name = token.contents.trim().to_owned();
                    result.push_str(&format!("%({})", var_name));
                    result.push('s');
                    vars.push(var_name);
                }
            }
            NodeEntry::Boxed(node) => {
                // Boxed VariableNodes possible via push() (not push_variable).
                if let Some(var_node) = node.as_variable_node()
                    && let Some(token) = var_node.token()
                {
                    let var_name = token.contents.trim().to_owned();
                    result.push_str(&format!("%({})", var_name));
                    result.push('s');
                    vars.push(var_name);
                }
            }
        }
    }

    (result, vars)
}

/// `%(name)s` interpolation using a pre-built data map. Mirrors
/// Django's `result %= data`.
fn interpolate_message_with_data(
    msg: &str,
    data: &HashMap<String, String>,
    context: &Context,
) -> String {
    let mut result = String::with_capacity(msg.len());
    let mut chars = msg.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            if chars.peek() == Some(&'(') {
                chars.next();
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ')' {
                        chars.next();
                        break;
                    }
                    name.push(c);
                    chars.next();
                }
                if let Some(&c) = chars.peek()
                    && (c == 's' || c == 'd' || c == 'r')
                {
                    chars.next();
                }
                if let Some(val) = data.get(&name) {
                    result.push_str(val);
                } else if let Some(val) = context.get(&name) {
                    result.push_str(&crate::nodes::render_value_in_context(
                        &val.clone(),
                        context,
                    ));
                } else {
                    // Django catches the KeyError and falls back; keep
                    // the placeholder text.
                    result.push_str(&format!("%({})", name));
                    result.push('s');
                }
            } else if chars.peek() == Some(&'%') {
                chars.next();
                result.push('%');
            } else {
                result.push('%');
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// `trimmed` option: strip each line and join with single spaces.
fn trim_message(msg: &str) -> String {
    msg.lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn compile_blocktranslate(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    let tag_name = &bits[0];

    let mut extra_context: Vec<(String, FilterExpression)> = Vec::new();
    let mut countervar = None;
    let mut counter = None;
    let mut message_context = None;
    let mut trimmed = false;
    let mut asvar = None;

    let mut i = 1;
    while i < bits.len() {
        match bits[i].as_str() {
            "with" => {
                i += 1;
                while i < bits.len() {
                    if let Some((name, expr)) = bits[i].split_once('=') {
                        let fe = parser.compile_filter(expr)?;
                        extra_context.push((name.to_owned(), fe));
                        i += 1;
                    } else if i + 2 < bits.len() && bits[i + 1] == "as" {
                        let fe = parser.compile_filter(&bits[i])?;
                        let name = bits[i + 2].clone();
                        extra_context.push((name, fe));
                        i += 3;
                        if i < bits.len() && bits[i] == "and" {
                            i += 1;
                        }
                    } else {
                        break;
                    }
                }
            }
            "count" => {
                if i + 1 >= bits.len() {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' tag 'count' option requires an argument.",
                        tag_name,
                    )));
                }
                i += 1;
                if let Some((name, expr)) = bits[i].split_once('=') {
                    countervar = Some(name.to_owned());
                    counter = Some(parser.compile_filter(expr)?);
                    i += 1;
                } else if i + 2 < bits.len() && bits[i + 1] == "as" {
                    let fe = parser.compile_filter(&bits[i])?;
                    countervar = Some(bits[i + 2].clone());
                    counter = Some(fe);
                    i += 3;
                } else {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' tag 'count' option requires a variable=expression.",
                        tag_name,
                    )));
                }
            }
            "context" => {
                if i + 1 >= bits.len() {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' tag 'context' option requires a value.",
                        tag_name,
                    )));
                }
                i += 1;
                message_context = Some(parser.compile_filter(&bits[i])?);
                i += 1;
            }
            "trimmed" => {
                trimmed = true;
                i += 1;
            }
            "asvar" => {
                if i + 1 >= bits.len() {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' tag 'asvar' option requires a variable name.",
                        tag_name,
                    )));
                }
                i += 1;
                asvar = Some(bits[i].clone());
                i += 1;
            }
            other => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Unknown argument for '{}' tag: '{}'.",
                    tag_name, other,
                )));
            }
        }
    }

    // Determine end tags based on whether plural is expected.
    let end_tag = if tag_name == "blocktrans" {
        "endblocktrans"
    } else {
        "endblocktranslate"
    };
    let plural_tag = "plural";

    let singular = parser.parse(&[plural_tag, end_tag])?;
    let next = parser.next_token();
    let next_cmd = next.contents.split_whitespace().next().unwrap_or("");

    let plural = if next_cmd == plural_tag {
        let pl = parser.parse(&[end_tag])?;
        parser.delete_first_token(); // consume end tag
        Some(pl)
    } else {
        // next_cmd == end_tag, already consumed
        None
    };

    if plural.is_some() && countervar.is_none() {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' tag with 'plural' requires a 'count' option.",
            tag_name,
        )));
    }

    Ok(Box::new(BlockTranslateNode {
        extra_context,
        singular,
        plural,
        countervar,
        counter,
        message_context,
        trimmed,
        asvar,
        token_field: None,
        origin_field: None,
    }))
}

// 3. {% language "code" %}

#[derive(Debug)]
struct LanguageNode {
    language_code: FilterExpression,
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for LanguageNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let code_val = resolve_expr(py, &self.language_code, context);
        let code_str = code_val.to_string();

        let translation = py
            .import("django.utils.translation")
            .map_err(|e| TemplateError::Internal(format!("Cannot import translation: {e}")))?;

        // Get the current language to restore later.
        let get_language = translation
            .getattr("get_language")
            .map_err(|e| TemplateError::Internal(format!("Cannot get get_language: {e}")))?;
        let old_language = get_language
            .call0()
            .map_err(|e| TemplateError::Internal(format!("get_language() failed: {e}")))?;

        // Activate the new language.
        let activate = translation
            .getattr("activate")
            .map_err(|e| TemplateError::Internal(format!("Cannot get activate: {e}")))?;
        activate
            .call1((code_str.as_str(),))
            .map_err(|e| TemplateError::Internal(format!("activate() failed: {e}")))?;

        // Render the body.
        let result = self.nodelist.render(py, context);

        // Restore the old language.
        if old_language.is_none() {
            let deactivate = translation
                .getattr("deactivate_all")
                .map_err(|e| TemplateError::Internal(format!("Cannot get deactivate_all: {e}")))?;
            let _ = deactivate.call0();
        } else {
            let _ = activate.call1((&old_language,));
        }

        result.map(|safe| safe.as_str().to_owned())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

pub fn compile_language(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() != 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'language' tag requires exactly one argument (the language code).".into(),
        ));
    }

    let language_code = parser.compile_filter(&bits[1])?;
    let nodelist = parser.parse(&["endlanguage"])?;
    parser.delete_first_token();

    Ok(Box::new(LanguageNode {
        language_code,
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}

// 4. {% get_current_language as var %}

#[derive(Debug)]
struct GetCurrentLanguageNode {
    variable: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for GetCurrentLanguageNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let translation = py
            .import("django.utils.translation")
            .map_err(|e| TemplateError::Internal(format!("Cannot import translation: {e}")))?;
        let get_language = translation
            .getattr("get_language")
            .map_err(|e| TemplateError::Internal(format!("Cannot get get_language: {e}")))?;
        let result = get_language
            .call0()
            .map_err(|e| TemplateError::Internal(format!("get_language() failed: {e}")))?;
        let lang: String = result
            .extract::<String>()
            .unwrap_or_else(|_| "en".to_owned());

        context.set(self.variable.clone(), Value::String(lang));
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_get_current_language(
    _parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    // {% get_current_language as var %}
    if bits.len() != 3 || bits[1] != "as" {
        return Err(TemplateError::TemplateSyntaxError(
            "'get_current_language' requires 'as variable' syntax.".into(),
        ));
    }

    Ok(Box::new(GetCurrentLanguageNode {
        variable: bits[2].clone(),
        token_field: None,
        origin_field: None,
    }))
}

// 5. {% get_current_language_bidi as var %}

#[derive(Debug)]
struct GetCurrentLanguageBidiNode {
    variable: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for GetCurrentLanguageBidiNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let translation = py
            .import("django.utils.translation")
            .map_err(|e| TemplateError::Internal(format!("Cannot import translation: {e}")))?;
        let get_language_bidi = translation
            .getattr("get_language_bidi")
            .map_err(|e| TemplateError::Internal(format!("Cannot get get_language_bidi: {e}")))?;
        let result = get_language_bidi
            .call0()
            .map_err(|e| TemplateError::Internal(format!("get_language_bidi() failed: {e}")))?;
        let bidi: bool = result.extract::<bool>().unwrap_or(false);

        context.set(self.variable.clone(), Value::Bool(bidi));
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_get_current_language_bidi(
    _parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    // {% get_current_language_bidi as var %}
    if bits.len() != 3 || bits[1] != "as" {
        return Err(TemplateError::TemplateSyntaxError(
            "'get_current_language_bidi' requires 'as variable' syntax.".into(),
        ));
    }

    Ok(Box::new(GetCurrentLanguageBidiNode {
        variable: bits[2].clone(),
        token_field: None,
        origin_field: None,
    }))
}

// 6. {% get_available_languages as var %}

#[derive(Debug)]
struct GetAvailableLanguagesNode {
    variable: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for GetAvailableLanguagesNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let settings = py
            .import("django.conf")
            .map_err(|e| TemplateError::Internal(format!("Cannot import django.conf: {e}")))?
            .getattr("settings")
            .map_err(|e| TemplateError::Internal(format!("Cannot get settings: {e}")))?;
        let languages = settings
            .getattr("LANGUAGES")
            .map_err(|e| TemplateError::Internal(format!("Cannot get LANGUAGES: {e}")))?;

        // LANGUAGES is a list of (code, name) tuples.
        let py_value = Value::from(&languages);
        context.set(self.variable.clone(), py_value);
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_get_available_languages(
    _parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    // {% get_available_languages as var %}
    if bits.len() != 3 || bits[1] != "as" {
        return Err(TemplateError::TemplateSyntaxError(
            "'get_available_languages' requires 'as variable' syntax.".into(),
        ));
    }

    Ok(Box::new(GetAvailableLanguagesNode {
        variable: bits[2].clone(),
        token_field: None,
        origin_field: None,
    }))
}

// 7. {% get_language_info for "code" as var %}

#[derive(Debug)]
struct GetLanguageInfoNode {
    lang_code: FilterExpression,
    variable: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for GetLanguageInfoNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let code_val = resolve_expr(py, &self.lang_code, context);
        let code_str = code_val.to_string();

        let translation = py
            .import("django.utils.translation")
            .map_err(|e| TemplateError::Internal(format!("Cannot import translation: {e}")))?;
        let get_language_info = translation
            .getattr("get_language_info")
            .map_err(|e| TemplateError::Internal(format!("Cannot get get_language_info: {e}")))?;
        let result = get_language_info
            .call1((code_str.as_str(),))
            .map_err(|e| TemplateError::Internal(format!("get_language_info() failed: {e}")))?;

        let py_value = Value::from(&result);
        context.set(self.variable.clone(), py_value);
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_get_language_info(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    // {% get_language_info for "code" as var %}
    if bits.len() != 5 || bits[1] != "for" || bits[3] != "as" {
        return Err(TemplateError::TemplateSyntaxError(
            "'get_language_info' requires 'for <code> as <variable>' syntax.".into(),
        ));
    }

    let lang_code = parser.compile_filter(&bits[2])?;

    Ok(Box::new(GetLanguageInfoNode {
        lang_code,
        variable: bits[4].clone(),
        token_field: None,
        origin_field: None,
    }))
}

// 8. {% get_language_info_list for codes as var %}

#[derive(Debug)]
struct GetLanguageInfoListNode {
    languages: FilterExpression,
    variable: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for GetLanguageInfoListNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let langs_val = resolve_expr(py, &self.languages, context);

        let translation = py
            .import("django.utils.translation")
            .map_err(|e| TemplateError::Internal(format!("Cannot import translation: {e}")))?;
        let get_language_info = translation
            .getattr("get_language_info")
            .map_err(|e| TemplateError::Internal(format!("Cannot get get_language_info: {e}")))?;

        // Resolve the iterable of language codes. Python lists (the
        // overwhelmingly common case - `{% get_language_info_list for
        // LANGUAGES as ... %}`) cross the FFI as `Value::PyObject`,
        // not `Value::List`, so the old single-arm match silently
        // stringified the entire list and asked
        // `get_language_info("['en', 'fr']")` - a guaranteed KeyError.
        let codes: Vec<String> = match &langs_val {
            Value::List(items) => items.iter().map(|item| item.to_string()).collect(),
            Value::PyObject(obj) => {
                let bound = obj.bind(py);
                let mut out = Vec::new();
                if let Ok(iter) = bound.try_iter() {
                    for item in iter.flatten() {
                        // Each item is either a code string or a
                        // 2-sequence `(code, name)`. Match Django's
                        // logic (`i18n.py:43-46`): if the first
                        // element's length is > 1, treat the whole
                        // item as a string code; otherwise the item
                        // is itself the code character (length 1)
                        // - i.e. we have a string code.
                        let code = if let Ok(s) = item.extract::<String>() {
                            s
                        } else {
                            // Sequence - take the first element.
                            match item.get_item(0).and_then(|v| v.extract::<String>()) {
                                Ok(s) => s,
                                Err(_) => item.to_string(),
                            }
                        };
                        out.push(code);
                    }
                }
                out
            }
            _ => vec![langs_val.to_string()],
        };

        let mut info_list = Vec::new();
        for code in &codes {
            match get_language_info.call1((code.as_str(),)) {
                Ok(result) => {
                    info_list.push(Value::from(&result));
                }
                Err(e) => {
                    return Err(TemplateError::Internal(format!(
                        "get_language_info('{}') failed: {}",
                        code, e,
                    )));
                }
            }
        }

        context.set(self.variable.clone(), Value::List(info_list));
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_get_language_info_list(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    // {% get_language_info_list for codes as var %}
    if bits.len() != 5 || bits[1] != "for" || bits[3] != "as" {
        return Err(TemplateError::TemplateSyntaxError(
            "'get_language_info_list' requires 'for <codes> as <variable>' syntax.".into(),
        ));
    }

    let languages = parser.compile_filter(&bits[2])?;

    Ok(Box::new(GetLanguageInfoListNode {
        languages,
        variable: bits[4].clone(),
        token_field: None,
        origin_field: None,
    }))
}

// Registration

/// Register all i18n template tags on the parser.
pub fn register_i18n_tags(parser: &mut Parser) {
    let tags: Vec<(&str, TagCompileFn)> = vec![
        ("translate", compile_translate),
        ("trans", compile_translate),
        ("blocktranslate", compile_blocktranslate),
        ("blocktrans", compile_blocktranslate),
        ("language", compile_language),
        ("get_current_language", compile_get_current_language),
        (
            "get_current_language_bidi",
            compile_get_current_language_bidi,
        ),
        ("get_available_languages", compile_get_available_languages),
        ("get_language_info", compile_get_language_info),
        ("get_language_info_list", compile_get_language_info_list),
    ];

    for (name, func) in tags {
        parser.tags.insert(
            name.to_owned(),
            crate::parser::TagCompileFunc::Rust(std::rc::Rc::new(func)),
        );
    }
}
