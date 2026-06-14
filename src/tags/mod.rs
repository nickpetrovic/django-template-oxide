//! Built-in template tags, port of `django.template.defaulttags`.

pub mod cache_tag;
pub mod for_tag;
pub mod i18n_tags;
pub mod loader_tags;
pub mod url_tag;

pub(crate) use cache_tag::compile_cache;
pub(crate) use for_tag::compile_for;
pub(crate) use url_tag::compile_url;

use std::collections::HashMap;

use once_cell::sync::Lazy;
use pyo3::prelude::*;
use regex::Regex;

use crate::context::{Context, ContextDict, Value};
use crate::errors::TemplateError;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::{Parser, TagCompileFn, TagCompileFunc};
use crate::smartif::{IfExpr, IfParser, IfValue, InfixOp, PrefixOp};
use crate::variable::FilterExpression;

#[allow(unused_imports)]
use crate::impl_node_metadata;

/// Resolve an `IfValue::Token` to a `Value`. For `{% if %}` conditions,
/// missing variables resolve to `Value::None` (Django's
/// `ignore_failures=True`), not to `string_if_invalid`.
pub(super) fn resolve_if_value(
    py: Python<'_>,
    fe: &FilterExpression,
    context: &mut Context,
) -> Value {
    let saved_sii = std::mem::take(&mut context.string_if_invalid);
    let result = crate::nodes::resolve_expression_ignore_failures(py, fe, context);
    context.string_if_invalid = saved_sii;

    match result {
        Ok(v) => v,
        Err(_) => Value::None,
    }
}

/// Determine the truthiness of a `Value`, matching Python/Django semantics.
fn value_is_truthy(value: &Value) -> bool {
    match value {
        Value::None => false,
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::SafeString(s) => !s.is_empty(),
        Value::List(items) => !items.is_empty(),
        Value::Dict(map) => !map.is_empty(),
        // Defer to Python's `bool()` for opaque PyObjects (ErrorList,
        // QuerySet, custom `__bool__`/`__len__`). Assuming truthy here
        // caused `{% if field.errors %}` to always fire.
        Value::PyObject(obj) => Python::attach(|py| obj.bind(py).is_truthy().unwrap_or(true)),
    }
}

/// Compare two `Value`s, returning an `Ordering` for comparison operators.
/// Returns `None` if the values are not comparable.
fn value_compare(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    if let (Some(a), Some(b)) = (left.as_str(), right.as_str()) {
        return Some(a.cmp(b));
    }
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        _ => None,
    }
}

/// Check if `needle` is "in" `haystack`, matching Django's `in` operator.
fn value_in(needle: &Value, haystack: &Value) -> bool {
    if let Some(haystack_str) = haystack.as_str() {
        return needle.as_str().is_some_and(|n| haystack_str.contains(n));
    }
    match haystack {
        Value::List(items) => items.iter().any(|item| item == needle),
        Value::Dict(map) => {
            if let Some(k) = needle.as_str() {
                map.contains_key(k)
            } else {
                false
            }
        }
        // Delegate `in` on PyObjects to Python's `__contains__`.
        Value::PyObject(obj) => Python::attach(|py| {
            let bound = obj.bind(py);
            let needle_obj = needle.to_pyobject(py);
            bound.contains(needle_obj.bind(py)).unwrap_or(false)
        }),
        _ => false,
    }
}

/// A compiled if-expression condition: pairs a token string with its
/// parsed `FilterExpression` for render-time resolution.
#[derive(Debug, Clone)]
pub struct CompiledIfExpr {
    expr: IfExpr,
    /// Map from token strings to their compiled `FilterExpression`.
    vars: HashMap<String, FilterExpression>,
}

impl CompiledIfExpr {
    /// If this condition is a single bare variable reference whose path
    /// starts with `loopvar` and has a batch_slot stamped on it, return
    /// that slot. Used by the BodyProgram compiler to emit a
    /// `JmpIfSlotFalsy` for the common `{% if app.is_archived %}` pattern.
    pub fn single_var_batch_slot(&self, loopvar: &str) -> Option<u16> {
        use crate::variable::FilterExpressionVar;

        let token = match &self.expr {
            IfExpr::Literal(IfValue::Token(t)) => t,
            _ => return None,
        };
        let fe = self.vars.get(token)?;
        if !fe.filters.is_empty() {
            return None;
        }
        let var = match &fe.var {
            FilterExpressionVar::Var(v) => v,
            _ => return None,
        };
        let parts = var.lookups()?;
        if parts.len() < 2 || parts[0] != loopvar {
            return None;
        }
        var.batch_slot()
    }
}

/// Evaluate an `IfExpr` AST node against a context.
fn eval_if_expr(
    py: Python<'_>,
    expr: &IfExpr,
    vars: &HashMap<String, FilterExpression>,
    context: &mut Context,
) -> bool {
    match expr {
        IfExpr::Literal(IfValue::Token(token)) => {
            if let Some(fe) = vars.get(token) {
                let val = resolve_if_value(py, fe, context);
                value_is_truthy(&val)
            } else {
                false
            }
        }
        IfExpr::Prefix {
            op: PrefixOp::Not,
            operand,
        } => !eval_if_expr(py, operand, vars, context),
        IfExpr::Infix { op, left, right } => match op {
            InfixOp::And => {
                eval_if_expr(py, left, vars, context) && eval_if_expr(py, right, vars, context)
            }
            InfixOp::Or => {
                eval_if_expr(py, left, vars, context) || eval_if_expr(py, right, vars, context)
            }
            _ => {
                let left_val = eval_if_expr_to_value(py, left, vars, context);
                let right_val = eval_if_expr_to_value(py, right, vars, context);
                match op {
                    InfixOp::Eq => left_val == right_val,
                    InfixOp::NotEq => left_val != right_val,
                    InfixOp::Gt => {
                        value_compare(&left_val, &right_val) == Some(std::cmp::Ordering::Greater)
                    }
                    InfixOp::Gte => matches!(
                        value_compare(&left_val, &right_val),
                        Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                    ),
                    InfixOp::Lt => {
                        value_compare(&left_val, &right_val) == Some(std::cmp::Ordering::Less)
                    }
                    InfixOp::Lte => matches!(
                        value_compare(&left_val, &right_val),
                        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                    ),
                    InfixOp::In => value_in(&left_val, &right_val),
                    InfixOp::NotIn => !value_in(&left_val, &right_val),
                    InfixOp::Is => left_val == right_val,
                    InfixOp::IsNot => left_val != right_val,
                    InfixOp::And | InfixOp::Or => unreachable!(),
                }
            }
        },
    }
}

/// Evaluate an `IfExpr` to a `Value` (for comparison operators).
fn eval_if_expr_to_value(
    py: Python<'_>,
    expr: &IfExpr,
    vars: &HashMap<String, FilterExpression>,
    context: &mut Context,
) -> Value {
    match expr {
        IfExpr::Literal(IfValue::Token(token)) => {
            if let Some(fe) = vars.get(token) {
                resolve_if_value(py, fe, context)
            } else {
                Value::String(String::new())
            }
        }
        IfExpr::Prefix {
            op: PrefixOp::Not,
            operand,
        } => Value::Bool(!eval_if_expr(py, operand, vars, context)),
        IfExpr::Infix {
            op: InfixOp::And,
            left,
            right,
        } => {
            if eval_if_expr(py, left, vars, context) {
                eval_if_expr_to_value(py, right, vars, context)
            } else {
                eval_if_expr_to_value(py, left, vars, context)
            }
        }
        IfExpr::Infix {
            op: InfixOp::Or,
            left,
            right,
        } => {
            if eval_if_expr(py, left, vars, context) {
                eval_if_expr_to_value(py, left, vars, context)
            } else {
                eval_if_expr_to_value(py, right, vars, context)
            }
        }
        IfExpr::Infix { .. } => Value::Bool(eval_if_expr(py, expr, vars, context)),
    }
}

/// Collect all variable token strings from an `IfExpr` AST.
fn collect_if_vars(expr: &IfExpr) -> Vec<String> {
    let mut result = Vec::new();
    collect_if_vars_inner(expr, &mut result);
    result
}

fn collect_if_vars_inner(expr: &IfExpr, out: &mut Vec<String>) {
    match expr {
        IfExpr::Literal(IfValue::Token(s)) => {
            out.push(s.clone());
        }
        IfExpr::Prefix { operand, .. } => {
            collect_if_vars_inner(operand, out);
        }
        IfExpr::Infix { left, right, .. } => {
            collect_if_vars_inner(left, out);
            collect_if_vars_inner(right, out);
        }
    }
}

// {% if %} / {% elif %} / {% else %} / {% endif %}

#[derive(Debug)]
pub struct IfBranch {
    /// `None` for `{% else %}`, `Some(...)` for `{% if %}` / `{% elif %}`.
    condition: Option<CompiledIfExpr>,
    nodelist: NodeList,
}

impl IfBranch {
    pub fn condition(&self) -> Option<&CompiledIfExpr> {
        self.condition.as_ref()
    }
    pub fn nodelist(&self) -> &NodeList {
        &self.nodelist
    }
}

#[derive(Debug)]
pub struct IfNode {
    branches: Vec<IfBranch>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl IfNode {
    pub fn branches(&self) -> &[IfBranch] {
        &self.branches
    }
}

impl Node for IfNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        for branch in &self.branches {
            let should_render = match &branch.condition {
                None => true, // {% else %}
                Some(compiled) => eval_if_expr(py, &compiled.expr, &compiled.vars, context),
            };
            if should_render {
                let safe = branch.nodelist.render(py, context)?;
                return Ok(safe.as_str().to_owned());
            }
        }
        Ok(String::new())
    }

    /// Stream matched branch children into the surrounding buffer,
    /// avoiding the intermediate `SafeString` + `to_owned`.
    #[inline]
    fn render_annotated_into(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        for branch in &self.branches {
            let should_render = match &branch.condition {
                None => true, // {% else %}
                Some(compiled) => eval_if_expr(py, &compiled.expr, &compiled.vars, context),
            };
            if should_render {
                return branch.nodelist.render_into(py, context, out);
            }
        }
        Ok(())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        for branch in &self.branches {
            visit(&branch.nodelist);
        }
    }
}

pub fn compile_if(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let mut branches: Vec<IfBranch> = Vec::new();

    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'if' statement requires at least one argument.".into(),
        ));
    }

    let condition = parse_if_condition(parser, &bits[1..])?;
    let nodelist = parser.parse(&["elif", "else", "endif"])?;
    branches.push(IfBranch {
        condition: Some(condition),
        nodelist,
    });

    loop {
        let next_token = parser.next_token();
        let next_bits = next_token.split_contents();
        let tag_name = next_bits[0].as_str();

        match tag_name {
            "elif" => {
                if next_bits.len() < 2 {
                    return Err(TemplateError::TemplateSyntaxError(
                        "'elif' statement requires at least one argument.".into(),
                    ));
                }
                let condition = parse_if_condition(parser, &next_bits[1..])?;
                let nodelist = parser.parse(&["elif", "else", "endif"])?;
                branches.push(IfBranch {
                    condition: Some(condition),
                    nodelist,
                });
            }
            "else" => {
                let nodelist = parser.parse(&["endif"])?;
                branches.push(IfBranch {
                    condition: None,
                    nodelist,
                });
                parser.delete_first_token();
                break;
            }
            "endif" => {
                break;
            }
            _ => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Unexpected tag '{}' in if block.",
                    tag_name,
                )));
            }
        }
    }

    Ok(Box::new(IfNode {
        branches,
        token_field: None,
        origin_field: None,
    }))
}

fn parse_if_condition(parser: &Parser, bits: &[String]) -> Result<CompiledIfExpr, TemplateError> {
    let bit_refs: Vec<&str> = bits.iter().map(|s| s.as_str()).collect();
    let mut if_parser = IfParser::new(&bit_refs);
    let expr = if_parser.parse()?;

    let token_strings = collect_if_vars(&expr);
    let mut vars = HashMap::new();
    for token_str in token_strings {
        if let std::collections::hash_map::Entry::Vacant(e) = vars.entry(token_str.clone()) {
            let fe = parser.compile_filter(&token_str)?;
            e.insert(fe);
        }
    }

    Ok(CompiledIfExpr { expr, vars })
}

// {% with %} / {% endwith %}

#[derive(Debug)]
struct WithNode {
    /// Extra variables to push: `(name, expression)`.
    extra_context: Vec<(String, FilterExpression)>,
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for WithNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let mut values: ContextDict = HashMap::new();
        for (name, fe) in &self.extra_context {
            let val = resolve_if_value(py, fe, context);
            values.insert(name.clone(), val);
        }

        context.push_with(values);
        let safe = self.nodelist.render(py, context)?;
        context.pop();

        Ok(safe.as_str().to_owned())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

pub fn compile_with(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    let mut extra_context: Vec<(String, FilterExpression)> = Vec::new();

    // Legacy: {% with expr as var %}
    if bits.len() == 4 && bits[2] == "as" {
        let fe = parser.compile_filter(&bits[1])?;
        extra_context.push((bits[3].clone(), fe));
    } else if bits.len() >= 2 {
        // {% with var1=expr1 var2=expr2 %}
        for bit in &bits[1..] {
            if let Some((name, expr)) = bit.split_once('=') {
                let fe = parser.compile_filter(expr)?;
                extra_context.push((name.to_owned(), fe));
            } else {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "'with' tag expected keyword arguments, got '{}'.",
                    bit,
                )));
            }
        }
    } else {
        return Err(TemplateError::TemplateSyntaxError(
            "'with' tag requires at least one argument.".into(),
        ));
    }

    let nodelist = parser.parse(&["endwith"])?;
    parser.delete_first_token();

    Ok(Box::new(WithNode {
        extra_context,
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}

// {% comment %} / {% endcomment %}

#[derive(Debug)]
struct CommentNode {
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for CommentNode {
    fn render(&self, _py: Python<'_>, _context: &mut Context) -> Result<String, TemplateError> {
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_comment(
    parser: &mut Parser,
    _token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    parser.skip_past("endcomment")?;

    Ok(Box::new(CommentNode {
        token_field: None,
        origin_field: None,
    }))
}

// {% autoescape on/off %} / {% endautoescape %}

#[derive(Debug)]
struct AutoEscapeNode {
    setting: bool,
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for AutoEscapeNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let old = context.autoescape;
        context.autoescape = self.setting;
        let safe = self.nodelist.render(py, context)?;
        context.autoescape = old;
        Ok(safe.as_str().to_owned())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

pub fn compile_autoescape(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() != 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'autoescape' tag requires exactly one argument: 'on' or 'off'.".into(),
        ));
    }

    let setting = match bits[1].as_str() {
        "on" => true,
        "off" => false,
        _ => {
            return Err(TemplateError::TemplateSyntaxError(format!(
                "'autoescape' argument should be 'on' or 'off', not '{}'.",
                bits[1],
            )));
        }
    };

    let nodelist = parser.parse(&["endautoescape"])?;
    parser.delete_first_token();

    Ok(Box::new(AutoEscapeNode {
        setting,
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}

// {% verbatim %} / {% endverbatim %}
//
// The lexer emits inner content as Text tokens. The parser only needs to
// consume the block, but the compile function must still be registered so
// {% verbatim %} tokens are recognized.

#[derive(Debug)]
struct VerbatimNode {
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for VerbatimNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let safe = self.nodelist.render(py, context)?;
        Ok(safe.as_str().to_owned())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

pub fn compile_verbatim(
    parser: &mut Parser,
    _token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    // The lexer handles matching `{% endverbatim ... %}` to the opener,
    // filtering non-matching content into TEXT tokens. So parsing until
    // `"endverbatim"` works for both bare and named-label forms,
    // matching Django's `parser.parse(("endverbatim",))`.
    let nodelist = parser.parse(&["endverbatim"])?;
    parser.delete_first_token();

    Ok(Box::new(VerbatimNode {
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}

// {% spaceless %} / {% endspaceless %}

static SPACELESS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r">\s+<").expect("SPACELESS_RE must compile"));

#[derive(Debug)]
struct SpacelessNode {
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for SpacelessNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let safe = self.nodelist.render(py, context)?;
        let trimmed = safe.as_str().trim();
        let stripped = strip_spaces_between_tags(trimmed);
        Ok(stripped)
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

/// Mirrors `django.utils.html.strip_spaces_between_tags`.
fn strip_spaces_between_tags(value: &str) -> String {
    SPACELESS_RE.replace_all(value, "><").to_string()
}

pub fn compile_spaceless(
    parser: &mut Parser,
    _token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let nodelist = parser.parse(&["endspaceless"])?;
    parser.delete_first_token();

    Ok(Box::new(SpacelessNode {
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}

// {% templatetag %}

#[derive(Debug)]
struct TemplateTagNode {
    tag_type: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for TemplateTagNode {
    fn render(&self, _py: Python<'_>, _context: &mut Context) -> Result<String, TemplateError> {
        let output = match self.tag_type.as_str() {
            "openblock" => "{%",
            "closeblock" => "%}",
            "openvariable" => "{{",
            "closevariable" => "}}",
            "openbrace" => "{",
            "closebrace" => "}",
            "opencomment" => "{#",
            "closecomment" => "#}",
            _ => "", // validated at parse time
        };
        Ok(output.to_owned())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

const TEMPLATETAG_TYPES: &[&str] = &[
    "openblock",
    "closeblock",
    "openvariable",
    "closevariable",
    "openbrace",
    "closebrace",
    "opencomment",
    "closecomment",
];

pub fn compile_templatetag(
    _parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() != 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'templatetag' statement takes one argument.".into(),
        ));
    }

    let tag_type = &bits[1];
    if !TEMPLATETAG_TYPES.contains(&tag_type.as_str()) {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "Invalid templatetag argument: '{}'. Must be one of: {}.",
            tag_type,
            TEMPLATETAG_TYPES.join(", "),
        )));
    }

    Ok(Box::new(TemplateTagNode {
        tag_type: tag_type.clone(),
        token_field: None,
        origin_field: None,
    }))
}

// {% firstof %}

#[derive(Debug)]
struct FirstOfNode {
    vars: Vec<FilterExpression>,
    asvar: Option<String>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for FirstOfNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        use crate::nodes::render_value_in_context;

        for fe in &self.vars {
            let val = resolve_if_value(py, fe, context);
            if value_is_truthy(&val) {
                let rendered = render_value_in_context(&val, context);
                if let Some(ref asvar) = self.asvar {
                    context.set(asvar.clone(), Value::SafeString(rendered.into()));
                    return Ok(String::new());
                }
                return Ok(rendered);
            }
        }

        if let Some(ref asvar) = self.asvar {
            context.set(asvar.clone(), Value::String(String::new()));
        }
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_firstof(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'firstof' statement requires at least one argument.".into(),
        ));
    }

    let mut end = bits.len();
    let mut asvar = None;

    if bits.len() >= 4 && bits[bits.len() - 2] == "as" {
        asvar = Some(bits[bits.len() - 1].clone());
        end = bits.len() - 2;
    }

    let mut vars = Vec::new();
    for bit in &bits[1..end] {
        let fe = parser.compile_filter(bit)?;
        vars.push(fe);
    }

    Ok(Box::new(FirstOfNode {
        vars,
        asvar,
        token_field: None,
        origin_field: None,
    }))
}

// {% cycle %}

#[derive(Debug)]
struct CycleNode {
    cyclevars: Vec<FilterExpression>,
    /// If set, the cycle value is stored in this context variable.
    variable_name: Option<String>,
    /// If true, suppress output (still advance and store in context).
    silent: bool,
    /// Unique render-context key for the cycle counter.
    render_key: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for CycleNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        use crate::nodes::render_value_in_context;

        let cycle_key = &self.render_key;
        let current_index = match context.render_context.get(cycle_key) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };

        let val = if !self.cyclevars.is_empty() {
            let idx = current_index % self.cyclevars.len();
            let fe = &self.cyclevars[idx];
            if fe.filters.is_empty() {
                resolve_if_value(py, fe, context)
            } else {
                use crate::filters::get_default_filters;
                let native_filters = get_default_filters();
                let mut obj = resolve_if_value(py, fe, context);
                for parsed_filter in &fe.filters {
                    if let Some(native) = native_filters.get(&parsed_filter.name) {
                        let mut arg_vals: Vec<Value> = Vec::new();
                        for arg in &parsed_filter.args {
                            if !arg.is_lookup {
                                arg_vals.push(match &arg.constant {
                                    Some(s) => Value::SafeString(s.clone().into()),
                                    None => Value::None,
                                });
                            } else {
                                let var = arg.variable.as_ref().unwrap();
                                let parts: Vec<&str> = var.var.split('.').collect();
                                let resolved = match context.get(parts[0]) {
                                    Some(v) => v.clone(),
                                    None => Value::String(String::new()),
                                };
                                arg_vals.push(resolved);
                            }
                        }
                        let autoescape = if native.needs_autoescape {
                            context.autoescape
                        } else {
                            false
                        };
                        let was_safe = matches!(&obj, Value::SafeString(_));
                        let result = (native.func)(&obj, &arg_vals, autoescape);
                        obj = if native.is_safe && was_safe {
                            match result {
                                Value::String(s) => Value::SafeString(s.into()),
                                other => other,
                            }
                        } else {
                            result
                        };
                    }
                }
                obj
            }
        } else {
            Value::String(String::new())
        };

        context
            .render_context
            .set(cycle_key.clone(), Value::Int((current_index + 1) as i64));

        let rendered = render_value_in_context(&val, context);

        if let Some(ref var_name) = self.variable_name {
            // set_upward (not set) so the cycle var persists across
            // {% with %} push/pop cycles. set() would lose it when the
            // {% with %} scope pops.
            context.set_upward(var_name, Value::String(rendered.clone()));
        }

        if self.silent {
            Ok(String::new())
        } else {
            Ok(rendered)
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

static CYCLE_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn compile_cycle(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'cycle' tag requires at least two arguments".into(),
        ));
    }

    // Single argument referencing a named cycle: {% cycle name %}
    if bits.len() == 2 {
        let name = &bits[1];
        match parser.named_cycles.get(name) {
            Some(state) => {
                return Ok(Box::new(CycleNode {
                    cyclevars: state.cyclevars.clone(),
                    variable_name: Some(name.clone()),
                    silent: state.silent,
                    render_key: state.render_key.clone(),
                    token_field: None,
                    origin_field: None,
                }));
            }
            None => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "No named cycles in template. '{}' is not defined",
                    name,
                )));
            }
        }
    }

    // {% cycle a b c [as varname [silent]] %}
    let mut end = bits.len();
    let mut variable_name = None;
    let mut silent = false;

    if let Some(as_pos) = bits.iter().position(|s| s == "as") {
        if as_pos + 1 >= bits.len() {
            return Err(TemplateError::TemplateSyntaxError(
                "'cycle' tag with 'as' requires a variable name.".into(),
            ));
        }
        variable_name = Some(bits[as_pos + 1].clone());
        end = as_pos;

        if as_pos + 2 < bits.len() {
            let flag = &bits[as_pos + 2];
            if flag == "silent" {
                silent = true;
                if as_pos + 3 < bits.len() {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "Only 'silent' flag is allowed after cycle's name, not '{}'.",
                        &bits[as_pos + 3],
                    )));
                }
            } else {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Only 'silent' flag is allowed after cycle's name, not '{}'.",
                    flag,
                )));
            }
        }
    }

    let mut cyclevars = Vec::new();
    for bit in &bits[1..end] {
        let fe = parser.compile_filter(bit)?;
        cyclevars.push(fe);
    }

    if cyclevars.len() < 2 && variable_name.is_none() {
        return Err(TemplateError::TemplateSyntaxError(
            "'cycle' tag requires at least two arguments".into(),
        ));
    }

    let render_key = format!(
        "__cycle_{}",
        CYCLE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );

    // Record so later `{% cycle NAME %}` and `{% resetcycle NAME %}`
    // resolve to the same cycle.
    if let Some(ref name) = variable_name {
        parser.named_cycles.insert(
            name.clone(),
            crate::parser::NamedCycleState {
                render_key: render_key.clone(),
                cyclevars: cyclevars.clone(),
                silent,
            },
        );
    }

    // For argument-less `{% resetcycle %}`.
    parser.last_cycle_render_key = Some(render_key.clone());

    Ok(Box::new(CycleNode {
        cyclevars,
        variable_name,
        silent,
        render_key,
        token_field: None,
        origin_field: None,
    }))
}

// {% load %}

#[derive(Debug)]
struct LoadNode {
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for LoadNode {
    fn render(&self, _py: Python<'_>, _context: &mut Context) -> Result<String, TemplateError> {
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_load(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'load' tag requires at least one argument.".into(),
        ));
    }

    let from_index = bits.iter().position(|s| s == "from");

    if let Some(fi) = from_index {
        // {% load tag1 tag2 from library %}
        let lib_name = bits.get(fi + 1).ok_or_else(|| {
            TemplateError::TemplateSyntaxError(
                "'load' tag expected a library name after 'from'.".into(),
            )
        })?;

        Python::attach(|py| -> Result<(), TemplateError> {
            let lib = resolve_library(parser, py, lib_name)?;
            register_library_tags(parser, py, &lib, Some(&bits[1..fi]))?;
            Ok(())
        })?;
    } else {
        // {% load library1 library2 %}
        for lib_name in &bits[1..] {
            Python::attach(|py| -> Result<(), TemplateError> {
                let lib = resolve_library(parser, py, lib_name)?;
                register_library_tags(parser, py, &lib, None)?;
                Ok(())
            })?;
        }
    }

    Ok(Box::new(LoadNode {
        token_field: None,
        origin_field: None,
    }))
}

/// Resolve a `{% load %}` library name to its `Library` object.
/// Mirrors Django's `find_library(parser, name)`: parser-registered
/// libraries (from `Engine.template_libraries`) take precedence, with
/// fallback to module-path import for engine-less compiles.
fn resolve_library<'py>(
    parser: &Parser,
    py: Python<'py>,
    name: &str,
) -> Result<Bound<'py, PyAny>, TemplateError> {
    if let Some(lib) = parser.libraries.get(name) {
        return Ok(lib.clone_ref(py).into_bound(py));
    }
    load_python_library(py, name)
}

/// Load a Django template tag library by name via Python.
/// Tries `django.templatetags.<name>` first, then the bare name, then
/// installed apps. Independent of `TEMPLATES` configuration.
fn load_python_library<'py>(
    py: Python<'py>,
    name: &str,
) -> Result<Bound<'py, PyAny>, TemplateError> {
    let lib_module = py.import("django.template.library").map_err(|e| {
        TemplateError::Internal(format!("Cannot import django.template.library: {e}"))
    })?;
    let import_library = lib_module
        .getattr("import_library")
        .map_err(|e| TemplateError::Internal(format!("Cannot get import_library: {e}")))?;

    let dotted_path = format!("django.templatetags.{}", name);
    if let Ok(lib) = import_library.call1((dotted_path.as_str(),)) { return Ok(lib) }

    match import_library.call1((name,)) {
        Ok(lib) => Ok(lib),
        Err(_) => {
            let backends = py
                .import("django.template.backends.django")
                .map_err(|e| TemplateError::Internal(format!("Cannot import backends: {e}")))?;
            let get_installed = backends.getattr("get_installed_libraries").map_err(|e| {
                TemplateError::Internal(format!("Cannot get get_installed_libraries: {e}"))
            })?;
            let installed = get_installed.call0().map_err(|e| {
                TemplateError::Internal(format!("get_installed_libraries failed: {e}"))
            })?;

            match installed.get_item(name) {
                Ok(full_path) => {
                    let full_path_str: String = full_path.extract().map_err(|e| {
                        TemplateError::Internal(format!("Cannot extract path: {e}"))
                    })?;
                    import_library
                        .call1((full_path_str.as_str(),))
                        .map_err(|_| {
                            TemplateError::TemplateSyntaxError(format!(
                                "'{}' is not a registered tag library. Could not import '{}'.",
                                name, full_path_str,
                            ))
                        })
                }
                Err(_) => Err(TemplateError::TemplateSyntaxError(format!(
                    "'{}' is not a registered tag library.",
                    name,
                ))),
            }
        }
    }
}

/// Register tags from a Python `Library` object into the parser.
/// Mirrors `Parser.add_library` (base.py:668). If `specific_tags` is
/// `Some`, only the named entries are registered (`{% load tag1 from lib %}`).
fn register_library_tags(
    parser: &mut Parser,
    py: Python<'_>,
    lib: &Bound<'_, PyAny>,
    specific_tags: Option<&[String]>,
) -> Result<(), TemplateError> {
    if specific_tags.is_none() {
        return parser
            .add_python_library(py, lib)
            .map_err(|e| TemplateError::Internal(format!("add_python_library: {e}")));
    }

    // {% load tag1 tag2 from library %}: pull only named entries.
    use pyo3::types::PyDict;
    let names = specific_tags.expect("checked above");

    let lib_tags_attr = lib
        .getattr(pyo3::intern!(py, "tags"))
        .map_err(|e| TemplateError::Internal(format!("Library.tags: {e}")))?;
    let lib_tags = lib_tags_attr
        .cast::<PyDict>()
        .map_err(|_| TemplateError::Internal("Library.tags is not a dict".into()))?;

    let lib_filters_attr = lib
        .getattr(pyo3::intern!(py, "filters"))
        .map_err(|e| TemplateError::Internal(format!("Library.filters: {e}")))?;
    let lib_filters = lib_filters_attr
        .cast::<PyDict>()
        .map_err(|_| TemplateError::Internal("Library.filters is not a dict".into()))?;

    let mut found_any = false;
    for name in names {
        if let Some(tag) = lib_tags.get_item(name.as_str()).ok().flatten() {
            parser.tags.insert(
                name.clone(),
                crate::parser::TagCompileFunc::Python(tag.unbind()),
            );
            found_any = true;
        }
        if let Some(filter) = lib_filters.get_item(name.as_str()).ok().flatten() {
            parser.filters.insert(name.clone(), filter.unbind());
            found_any = true;
        }
    }

    if !found_any {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' is not a valid tag or filter in the loaded library",
            names.join("', '")
        )));
    }
    Ok(())
}

// {% now %}

#[derive(Debug)]
struct NowNode {
    format_string: FilterExpression,
    asvar: Option<String>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for NowNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let format_val = resolve_if_value(py, &self.format_string, context);
        let format_str = format_val.to_string();

        // Use Django's `date` filter so format names like DATE_FORMAT
        // resolve via django.utils.formats.date_format().
        let formatted: String = (|| {
            let date_filter = py
                .import("django.template.defaultfilters")
                .map_err(|e| TemplateError::Internal(format!("Cannot import defaultfilters: {e}")))?
                .getattr("date")
                .map_err(|e| TemplateError::Internal(format!("Cannot get date filter: {e}")))?;
            let datetime_mod = py
                .import("datetime")
                .map_err(|e| TemplateError::Internal(format!("Cannot import datetime: {e}")))?;
            let settings = py
                .import("django.conf")
                .map_err(|e| TemplateError::Internal(format!("{e}")))?
                .getattr("settings")
                .map_err(|e| TemplateError::Internal(format!("{e}")))?;
            let use_tz = settings
                .getattr("USE_TZ")
                .and_then(|v| v.extract::<bool>())
                .unwrap_or(false);
            let now = if use_tz {
                let tz_mod = py
                    .import("django.utils.timezone")
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?;
                let tz = tz_mod
                    .call_method0("get_current_timezone")
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?;
                datetime_mod
                    .getattr("datetime")
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?
                    .call_method1("now", (&tz,))
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?
            } else {
                datetime_mod
                    .getattr("datetime")
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?
                    .call_method0("now")
                    .map_err(|e| TemplateError::Internal(format!("{e}")))?
            };
            let result = date_filter
                .call1((&now, format_str.as_str()))
                .map_err(|e| TemplateError::Internal(format!("date filter failed: {e}")))?;
            result
                .extract::<String>()
                .map_err(|e| TemplateError::Internal(format!("date filter result not string: {e}")))
        })()?;

        if let Some(ref asvar) = self.asvar {
            context.set(asvar.clone(), Value::String(formatted));
            Ok(String::new())
        } else {
            Ok(formatted)
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_now(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'now' statement requires one argument (format string).".into(),
        ));
    }

    let format_string = parser.compile_filter(&bits[1])?;

    let asvar = if bits.len() >= 4 && bits[2] == "as" {
        Some(bits[3].clone())
    } else {
        None
    };

    Ok(Box::new(NowNode {
        format_string,
        asvar,
        token_field: None,
        origin_field: None,
    }))
}

// {% filter %} / {% endfilter %}

#[derive(Debug)]
struct FilterNode {
    /// Filter chain wrapping a dummy `var` whose value is the rendered body.
    filter_expr: FilterExpression,
    nodelist: NodeList,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for FilterNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        // Body is already escaped per autoescape, so wrap as SafeString.
        let safe = self.nodelist.render(py, context)?;
        let content = Value::SafeString(safe.as_str().to_owned().into());

        // Bind `var` to the rendered content for the filter chain
        // (compiled at parse time as `var|<chain>`).
        let mut layer = ContextDict::new();
        layer.insert("var".to_owned(), content);
        context.push_with(layer);

        let result =
            crate::nodes::resolve_expression_ignore_failures(py, &self.filter_expr, context);
        context.pop();

        let val = result?;
        // Filters can return non-strings (length, default_if_none, etc.);
        // `Value::Display` matches Django's `force_str`.
        Ok(val.to_string())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

pub fn compile_filter(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'filter' tag requires at least one argument (the filter chain).".into(),
        ));
    }

    // Compile as `var|<chain>` (Django's `defaulttags.do_filter`). Plain
    // `var` keeps Variable parsing happy (leading underscores are rejected).
    let filter_chain = bits[1..].join(" ");
    let filter_token = format!("var|{}", filter_chain);
    let filter_expr = parser.compile_filter(&filter_token)?;

    let nodelist = parser.parse(&["endfilter"])?;
    parser.delete_first_token();

    Ok(Box::new(FilterNode {
        filter_expr,
        nodelist,
        token_field: None,
        origin_field: None,
    }))
}

// {% csrf_token %}

#[derive(Debug)]
struct CsrfTokenNode {
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for CsrfTokenNode {
    fn render(&self, _py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let csrf_token = match context.get("csrf_token") {
            Some(Value::String(s)) if !s.is_empty() && s != "NOTPROVIDED" => s.clone(),
            Some(Value::SafeString(s)) if !s.is_empty() && s.as_ref() != "NOTPROVIDED" => {
                s.as_ref().to_owned()
            }
            Some(Value::PyObject(obj)) => {
                let s = Python::attach(|py| {
                    obj.bind(py)
                        .str()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                if s.is_empty() || s == "NOTPROVIDED" {
                    return Ok(String::new());
                }
                s
            }
            _ => return Ok(String::new()),
        };

        Ok(format!(
            r#"<input type="hidden" name="csrfmiddlewaretoken" value="{}">"#,
            crate::utils::html_escape(&csrf_token),
        ))
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_csrf_token(
    _parser: &mut Parser,
    _token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    Ok(Box::new(CsrfTokenNode {
        token_field: None,
        origin_field: None,
    }))
}

// {% widthratio %}

#[derive(Debug)]
struct WidthRatioNode {
    val_expr: FilterExpression,
    max_expr: FilterExpression,
    max_width: FilterExpression,
    asvar: Option<String>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for WidthRatioNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let val = resolve_if_value(py, &self.val_expr, context);
        let max_val = resolve_if_value(py, &self.max_expr, context);
        let max_width_val = resolve_if_value(py, &self.max_width, context);

        let max_width = match try_to_int(&max_width_val) {
            Some(w) => w,
            None => {
                return Err(TemplateError::TemplateSyntaxError(
                    "widthratio final argument must be a number".into(),
                ));
            }
        };

        let val_f = match try_to_f64(&val) {
            Some(f) => f,
            None => return self.store_or_return("", context),
        };
        let max_f = match try_to_f64(&max_val) {
            Some(f) => f,
            None => return self.store_or_return("", context),
        };

        let result = if max_f == 0.0 {
            "0".to_owned()
        } else {
            let ratio = val_f / max_f * max_width as f64;
            if ratio.is_nan() || ratio.is_infinite() {
                String::new()
            } else {
                format!("{}", python_round(ratio))
            }
        };

        self.store_or_return(&result, context)
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

impl WidthRatioNode {
    fn store_or_return(&self, val: &str, context: &mut Context) -> Result<String, TemplateError> {
        if let Some(ref asvar) = self.asvar {
            context.set(asvar.clone(), Value::String(val.to_owned()));
            Ok(String::new())
        } else {
            Ok(val.to_owned())
        }
    }
}

fn try_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::String(s) => s.parse::<f64>().ok(),
        Value::SafeString(s) => s.parse::<f64>().ok(),
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        Value::None => None,
        _ => None,
    }
}

fn try_to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        Value::Float(f) => Some(*f as i64),
        Value::String(s) => s.parse::<i64>().ok(),
        Value::SafeString(s) => s.parse::<i64>().ok(),
        Value::Bool(true) => Some(1),
        Value::Bool(false) => Some(0),
        _ => None,
    }
}

fn python_round(x: f64) -> i64 {
    let r = x.round();
    let ri = r as i64;
    if (x.fract().abs() - 0.5).abs() < 1e-9 && ri % 2 != 0 {
        ri - x.signum() as i64
    } else {
        ri
    }
}

#[allow(dead_code)]
fn value_to_f64(value: &Value) -> f64 {
    try_to_f64(value).unwrap_or(0.0)
}

pub fn compile_widthratio(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    // {% widthratio value max_value max_width [as var] %}
    let bits = token.split_contents();
    if bits.len() < 4 {
        return Err(TemplateError::TemplateSyntaxError(
            "'widthratio' tag requires at least three arguments: value, max_value, max_width."
                .into(),
        ));
    }

    let val_expr = parser.compile_filter(&bits[1])?;
    let max_expr = parser.compile_filter(&bits[2])?;
    let max_width = parser.compile_filter(&bits[3])?;

    let asvar = if bits.len() >= 6 && bits[4] == "as" {
        Some(bits[5].clone())
    } else {
        None
    };

    Ok(Box::new(WidthRatioNode {
        val_expr,
        max_expr,
        max_width,
        asvar,
        token_field: None,
        origin_field: None,
    }))
}

// {% ifchanged %} / {% else %} / {% endifchanged %}

#[derive(Debug)]
struct IfChangedNode {
    /// Variables to compare. Empty means compare rendered content.
    vars: Vec<FilterExpression>,
    nodelist_true: NodeList,
    nodelist_false: Option<NodeList>,
    render_key: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

static IFCHANGED_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Node for IfChangedNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let current: String = if self.vars.is_empty() {
            let safe = self.nodelist_true.render(py, context)?;
            safe.as_str().to_owned()
        } else {
            let mut parts = Vec::with_capacity(self.vars.len());
            for fe in &self.vars {
                let val = resolve_if_value(py, fe, context);
                parts.push(val.to_string());
            }
            parts.join("\x01") // separator unlikely to appear in values
        };

        // Django stores ifchanged state in context['forloop'] when inside
        // a for loop, so it resets on each iteration of the parent loop.
        // Outside loops, it uses render_context (effectively a no-op since
        // the state is bound to self).
        let (last_value, use_forloop) = if let Some(Value::Dict(forloop)) = context.get("forloop") {
            (forloop.get(&self.render_key).cloned(), true)
        } else {
            (context.render_context.get(&self.render_key).cloned(), false)
        };

        let changed = match &last_value {
            Some(Value::String(s)) => s != &current,
            _ => true,
        };

        // Store updated state
        if use_forloop {
            if let Some(Value::Dict(forloop)) = context.base.get_mut("forloop") {
                forloop.insert(self.render_key.clone(), Value::String(current.clone()));
            }
        } else {
            context
                .render_context
                .set(self.render_key.clone(), Value::String(current.clone()));
        }

        if changed {
            if self.vars.is_empty() {
                Ok(current)
            } else {
                let safe = self.nodelist_true.render(py, context)?;
                Ok(safe.as_str().to_owned())
            }
        } else if let Some(ref nodelist_false) = self.nodelist_false {
            let safe = nodelist_false.render(py, context)?;
            Ok(safe.as_str().to_owned())
        } else {
            Ok(String::new())
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist_true", "nodelist_false"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist_true);
        if let Some(ref nl) = self.nodelist_false {
            visit(nl);
        }
    }
}

pub fn compile_ifchanged(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();

    let mut vars = Vec::new();
    for bit in &bits[1..] {
        let fe = parser.compile_filter(bit)?;
        vars.push(fe);
    }

    let nodelist_true = parser.parse(&["else", "endifchanged"])?;

    let next = parser.next_token();
    let tag = next.contents.split_whitespace().next().unwrap_or("");
    let nodelist_false = if tag == "else" {
        let nl = parser.parse(&["endifchanged"])?;
        parser.delete_first_token();
        Some(nl)
    } else {
        None
    };

    let render_key = format!(
        "__ifchanged_{}",
        IFCHANGED_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );

    Ok(Box::new(IfChangedNode {
        vars,
        nodelist_true,
        nodelist_false,
        render_key,
        token_field: None,
        origin_field: None,
    }))
}

// {% partialdef name [inline] %} / {% endpartialdef %}
// {% partial name %}
//
// Django 6.0 named fragments. The parser owns a shared
// `Arc<Mutex<HashMap<String, Arc<NodeList>>>>` so partialdef and partial
// can be declared in either order. Mirrors Django's
// `parser.extra_data.setdefault("partials", {})`.

#[derive(Debug)]
pub struct PartialDefNode {
    pub name: String,
    inline: bool,
    pub nodelist: std::sync::Arc<NodeList>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for PartialDefNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        if self.inline {
            let safe = self.nodelist.render(py, context)?;
            Ok(safe.as_str().to_owned())
        } else {
            Ok(String::new())
        }
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

pub fn compile_partialdef(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'partialdef' tag requires at least one argument (the partial name).".into(),
        ));
    }

    let name = bits[1].clone();
    let inline = bits.len() >= 3 && bits[2] == "inline";

    let nodelist = parser.parse(&["endpartialdef"])?;
    parser.delete_first_token();

    let arc_nodelist = std::sync::Arc::new(nodelist);

    if let Ok(mut map) = parser.partials.lock() {
        map.insert(name.clone(), std::sync::Arc::clone(&arc_nodelist));
    }

    Ok(Box::new(PartialDefNode {
        name,
        inline,
        nodelist: arc_nodelist,
        token_field: None,
        origin_field: None,
    }))
}

#[derive(Debug)]
struct PartialNode {
    name: String,
    /// Shared with the parser. Lookup deferred to render time so
    /// forward references work.
    partials: std::sync::Arc<std::sync::Mutex<HashMap<String, std::sync::Arc<NodeList>>>>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for PartialNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let nodelist = {
            let map = self
                .partials
                .lock()
                .map_err(|_| TemplateError::Internal("partials map mutex poisoned".into()))?;
            map.get(&self.name).cloned()
        };
        let nodelist = nodelist.ok_or_else(|| {
            TemplateError::TemplateSyntaxError(format!(
                "Partial '{}' is not defined in the current template.",
                self.name,
            ))
        })?;
        let safe = nodelist.render(py, context)?;
        Ok(safe.as_str().to_owned())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_partial(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    if bits.len() != 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'partial' tag requires exactly one argument (the partial name).".into(),
        ));
    }

    let name = bits[1].clone();

    // Defer validation to render time so forward references work.
    Ok(Box::new(PartialNode {
        name,
        partials: std::sync::Arc::clone(&parser.partials),
        token_field: None,
        origin_field: None,
    }))
}

// {% resetcycle [name] %}

#[derive(Debug)]
struct ResetCycleNode {
    render_key: String,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

impl Node for ResetCycleNode {
    fn render(&self, _py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        context
            .render_context
            .set(self.render_key.clone(), Value::Int(0));
        Ok(String::new())
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn compile_resetcycle(
    parser: &mut Parser,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();

    if bits.len() > 2 {
        return Err(TemplateError::TemplateSyntaxError(
            "'resetcycle' tag takes at most one argument.".into(),
        ));
    }

    let render_key = if bits.len() == 2 {
        let name = &bits[1];
        match parser.named_cycles.get(name) {
            Some(state) => state.render_key.clone(),
            None => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Named cycle '{}' does not exist",
                    name,
                )));
            }
        }
    } else {
        match &parser.last_cycle_render_key {
            Some(key) => key.clone(),
            None => {
                return Err(TemplateError::TemplateSyntaxError(
                    "'resetcycle' requires a preceding 'cycle' tag.".into(),
                ));
            }
        }
    };

    Ok(Box::new(ResetCycleNode {
        render_key,
        token_field: None,
        origin_field: None,
    }))
}

/// Register all built-in template tags on the parser.
pub fn register_default_tags(parser: &mut Parser) {
    let tags: Vec<(&str, TagCompileFn)> = vec![
        ("if", compile_if),
        ("for", compile_for),
        ("with", compile_with),
        ("comment", compile_comment),
        ("autoescape", compile_autoescape),
        ("verbatim", compile_verbatim),
        ("spaceless", compile_spaceless),
        ("templatetag", compile_templatetag),
        ("firstof", compile_firstof),
        ("cycle", compile_cycle),
        ("url", compile_url),
        ("load", compile_load),
        ("now", compile_now),
        ("filter", compile_filter),
        ("csrf_token", compile_csrf_token),
        ("widthratio", compile_widthratio),
        ("ifchanged", compile_ifchanged),
        ("cache", compile_cache),
        ("partialdef", compile_partialdef),
        ("partial", compile_partial),
        ("resetcycle", compile_resetcycle),
    ];

    for (name, func) in tags {
        parser
            .tags
            .insert(name.to_owned(), TagCompileFunc::Rust(std::rc::Rc::new(func)));
    }

    i18n_tags::register_i18n_tags(parser);
    loader_tags::register_loader_tags(parser);
}

#[cfg(test)]
#[allow(clippy::approx_constant)]
mod tests {
    use super::*;
    use crate::context::{Context, Value};
    use crate::lexer::Lexer;

    fn parse_template(source: &str) -> Result<NodeList, TemplateError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();
        let mut parser = Parser::new(tokens);
        register_default_tags(&mut parser);
        parser.parse(&[])
    }

    fn render(source: &str, vars: Vec<(&str, Value)>) -> Result<String, TemplateError> {
        let nodelist = parse_template(source)?;
        let ctx_dict: ContextDict = vars.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        let mut context = Context::new(Some(ctx_dict));
        Python::attach(|py| {
            let safe = nodelist.render(py, &mut context)?;
            Ok(safe.as_str().to_owned())
        })
    }

    #[test]
    fn test_if_true() {
        let result = render(
            "{% if show %}yes{% endif %}",
            vec![("show", Value::Bool(true))],
        );
        assert_eq!(result.unwrap(), "yes");
    }

    #[test]
    fn test_if_false() {
        let result = render(
            "{% if show %}yes{% endif %}",
            vec![("show", Value::Bool(false))],
        );
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_if_else() {
        let result = render(
            "{% if show %}yes{% else %}no{% endif %}",
            vec![("show", Value::Bool(false))],
        );
        assert_eq!(result.unwrap(), "no");
    }

    #[test]
    fn test_if_elif() {
        let result = render(
            "{% if a %}A{% elif b %}B{% else %}C{% endif %}",
            vec![("a", Value::Bool(false)), ("b", Value::Bool(true))],
        );
        assert_eq!(result.unwrap(), "B");
    }

    #[test]
    fn test_if_not() {
        let result = render(
            "{% if not show %}hidden{% endif %}",
            vec![("show", Value::Bool(false))],
        );
        assert_eq!(result.unwrap(), "hidden");
    }

    #[test]
    fn test_if_and() {
        let result = render(
            "{% if a and b %}both{% endif %}",
            vec![("a", Value::Bool(true)), ("b", Value::Bool(true))],
        );
        assert_eq!(result.unwrap(), "both");
    }

    #[test]
    fn test_if_or() {
        let result = render(
            "{% if a or b %}either{% endif %}",
            vec![("a", Value::Bool(false)), ("b", Value::Bool(true))],
        );
        assert_eq!(result.unwrap(), "either");
    }

    #[test]
    fn test_if_comparison() {
        let result = render("{% if x == 1 %}one{% endif %}", vec![("x", Value::Int(1))]);
        assert_eq!(result.unwrap(), "one");
    }

    #[test]
    fn test_if_string_truthy() {
        let result = render(
            "{% if name %}hello{% endif %}",
            vec![("name", Value::String("Alice".into()))],
        );
        assert_eq!(result.unwrap(), "hello");
    }

    #[test]
    fn test_if_empty_string_falsy() {
        let result = render(
            "{% if name %}hello{% else %}nobody{% endif %}",
            vec![("name", Value::String(String::new()))],
        );
        assert_eq!(result.unwrap(), "nobody");
    }

    #[test]
    fn test_for_basic() {
        let result = render(
            "{% for item in items %}{{ item }} {% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::String("a".into()),
                    Value::String("b".into()),
                    Value::String("c".into()),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "a b c ");
    }

    #[test]
    fn test_for_empty() {
        let result = render(
            "{% for item in items %}{{ item }}{% empty %}none{% endfor %}",
            vec![("items", Value::List(vec![]))],
        );
        assert_eq!(result.unwrap(), "none");
    }

    #[test]
    fn test_for_counter() {
        let result = render(
            "{% for item in items %}{{ forloop.counter }}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![Value::String("a".into()), Value::String("b".into())]),
            )],
        );
        assert_eq!(result.unwrap(), "12");
    }

    #[test]
    fn test_for_first_last() {
        let result = render(
            "{% for item in items %}{% if forloop.first %}F{% endif %}{% if forloop.last %}L{% endif %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::String("a".into()),
                    Value::String("b".into()),
                    Value::String("c".into()),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "FL");
    }

    #[test]
    fn test_for_reversed() {
        let result = render(
            "{% for item in items reversed %}{{ item }}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::String("a".into()),
                    Value::String("b".into()),
                    Value::String("c".into()),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "cba");
    }

    #[test]
    fn test_with_new_syntax() {
        let result = render(
            "{% with name='World' %}Hello {{ name }}{% endwith %}",
            vec![],
        );
        assert_eq!(result.unwrap(), "Hello World");
    }

    #[test]
    fn test_with_variable() {
        let result = render(
            "{% with greeting=name %}{{ greeting }}{% endwith %}",
            vec![("name", Value::String("Alice".into()))],
        );
        assert_eq!(result.unwrap(), "Alice");
    }

    #[test]
    fn test_with_scope_isolation() {
        let result = render(
            "{% with x='inner' %}{{ x }}{% endwith %}{{ x }}",
            vec![("x", Value::String("outer".into()))],
        );
        assert_eq!(result.unwrap(), "innerouter");
    }

    #[test]
    fn test_comment() {
        let result = render("before{% comment %}hidden{% endcomment %}after", vec![]);
        assert_eq!(result.unwrap(), "beforeafter");
    }

    #[test]
    fn test_autoescape_off() {
        let result = render(
            "{% autoescape off %}{{ html }}{% endautoescape %}",
            vec![("html", Value::String("<b>bold</b>".into()))],
        );
        assert_eq!(result.unwrap(), "<b>bold</b>");
    }

    #[test]
    fn test_autoescape_on() {
        let result = render(
            "{% autoescape on %}{{ html }}{% endautoescape %}",
            vec![("html", Value::String("<b>bold</b>".into()))],
        );
        assert_eq!(result.unwrap(), "&lt;b&gt;bold&lt;/b&gt;");
    }

    #[test]
    fn test_spaceless() {
        let result = render(
            "{% spaceless %}<p> \n </p> \n <p> </p>{% endspaceless %}",
            vec![],
        );
        assert_eq!(result.unwrap(), "<p></p><p></p>");
    }

    #[test]
    fn test_templatetag_openblock() {
        let result = render("{% templatetag openblock %}", vec![]);
        assert_eq!(result.unwrap(), "{%");
    }

    #[test]
    fn test_templatetag_closevariable() {
        let result = render("{% templatetag closevariable %}", vec![]);
        assert_eq!(result.unwrap(), "}}");
    }

    #[test]
    fn test_firstof_first_truthy() {
        let result = render(
            "{% firstof a b c %}",
            vec![
                ("a", Value::String(String::new())),
                ("b", Value::String("B".into())),
                ("c", Value::String("C".into())),
            ],
        );
        assert_eq!(result.unwrap(), "B");
    }

    #[test]
    fn test_firstof_none_truthy() {
        let result = render(
            "{% firstof a b %}",
            vec![
                ("a", Value::String(String::new())),
                ("b", Value::String(String::new())),
            ],
        );
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_csrf_token() {
        let result = render(
            "{% csrf_token %}",
            vec![("csrf_token", Value::String("abc123".into()))],
        );
        assert_eq!(
            result.unwrap(),
            r#"<input type="hidden" name="csrfmiddlewaretoken" value="abc123">"#,
        );
    }

    #[test]
    fn test_csrf_token_missing() {
        let result = render("{% csrf_token %}", vec![]);
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_csrf_token_not_provided() {
        let result = render(
            "{% csrf_token %}",
            vec![("csrf_token", Value::String("NOTPROVIDED".into()))],
        );
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_csrf_token_escapes_value() {
        let result = render(
            "{% csrf_token %}",
            vec![("csrf_token", Value::String("a<b>c".into()))],
        );
        let output = result.unwrap();
        assert!(output.contains("a&lt;b&gt;c"));
    }

    #[test]
    fn test_cycle_basic() {
        let result = render(
            "{% for item in items %}{% cycle 'a' 'b' 'c' %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(3),
                    Value::Int(4),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "abca");
    }

    #[test]
    fn test_if_missing_argument() {
        let result = parse_template("{% if %}yes{% endif %}");
        assert!(result.is_err());
    }

    #[test]
    fn test_for_missing_in() {
        let result = parse_template("{% for x %}{% endfor %}");
        assert!(result.is_err());
    }

    #[test]
    fn test_templatetag_invalid_type() {
        let result = parse_template("{% templatetag invalid %}");
        assert!(result.is_err());
    }

    #[test]
    fn test_autoescape_invalid_arg() {
        let result = parse_template("{% autoescape maybe %}{% endautoescape %}");
        assert!(result.is_err());
    }

    #[test]
    fn test_value_is_truthy() {
        assert!(!value_is_truthy(&Value::None));
        assert!(!value_is_truthy(&Value::Bool(false)));
        assert!(value_is_truthy(&Value::Bool(true)));
        assert!(!value_is_truthy(&Value::Int(0)));
        assert!(value_is_truthy(&Value::Int(1)));
        assert!(!value_is_truthy(&Value::String(String::new())));
        assert!(value_is_truthy(&Value::String("x".into())));
        assert!(!value_is_truthy(&Value::List(vec![])));
        assert!(value_is_truthy(&Value::List(vec![Value::Int(1)])));
    }

    #[test]
    fn test_strip_spaces_between_tags() {
        assert_eq!(strip_spaces_between_tags("<p> </p>"), "<p></p>");
        assert_eq!(
            strip_spaces_between_tags("<p>  \n  </p>  \n  <p>  </p>"),
            "<p></p><p></p>"
        );
        assert_eq!(strip_spaces_between_tags("<p>text</p>"), "<p>text</p>");
    }

    #[test]
    fn test_widthratio_basic() {
        let result = render("{% widthratio 175 200 100 %}", vec![]);
        assert_eq!(result.unwrap(), "88");
    }

    #[test]
    fn test_widthratio_zero_max() {
        let result = render("{% widthratio 175 0 100 %}", vec![]);
        assert_eq!(result.unwrap(), "0");
    }

    #[test]
    fn test_widthratio_with_variables() {
        let result = render(
            "{% widthratio val max_val width %}",
            vec![
                ("val", Value::Int(50)),
                ("max_val", Value::Int(100)),
                ("width", Value::Int(200)),
            ],
        );
        assert_eq!(result.unwrap(), "100");
    }

    #[test]
    fn test_widthratio_as_var() {
        let result = render("{% widthratio 175 200 100 as ratio %}{{ ratio }}", vec![]);
        assert_eq!(result.unwrap(), "88");
    }

    #[test]
    fn test_widthratio_rounding() {
        let result = render("{% widthratio 175 200 100 %}", vec![]);
        assert_eq!(result.unwrap(), "88");
    }

    #[test]
    fn test_widthratio_exact() {
        let result = render("{% widthratio 50 100 100 %}", vec![]);
        assert_eq!(result.unwrap(), "50");
    }

    #[test]
    fn test_ifchanged_with_variable() {
        let result = render(
            "{% for i in items %}{% ifchanged i %}{{ i }}{% endifchanged %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::Int(1),
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(2),
                    Value::Int(3),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "123");
    }

    #[test]
    fn test_ifchanged_without_variable() {
        let result = render(
            "{% for i in items %}{% ifchanged %}{{ i }}{% endifchanged %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::Int(1),
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(2),
                    Value::Int(3),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "123");
    }

    #[test]
    fn test_ifchanged_with_else() {
        let result = render(
            "{% for i in items %}{% ifchanged i %}C{% else %}S{% endifchanged %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::Int(1),
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(2),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "CSCS");
    }

    #[test]
    fn test_ifchanged_all_same() {
        let result = render(
            "{% for i in items %}{% ifchanged %}{{ i }}{% endifchanged %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::String("a".into()),
                    Value::String("a".into()),
                    Value::String("a".into()),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "a");
    }

    #[test]
    fn test_ifchanged_all_different() {
        let result = render(
            "{% for i in items %}{% ifchanged %}{{ i }}{% endifchanged %}{% endfor %}",
            vec![(
                "items",
                Value::List(vec![
                    Value::String("a".into()),
                    Value::String("b".into()),
                    Value::String("c".into()),
                ]),
            )],
        );
        assert_eq!(result.unwrap(), "abc");
    }

    #[test]
    fn test_widthratio_too_few_args() {
        let result = parse_template("{% widthratio 175 200 %}");
        assert!(result.is_err());
    }

    #[test]
    fn test_ifchanged_parse_basic() {
        let result = parse_template("{% ifchanged %}x{% endifchanged %}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_ifchanged_parse_with_else() {
        let result = parse_template("{% ifchanged x %}C{% else %}S{% endifchanged %}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_cache_parse_basic() {
        let result = parse_template("{% cache 300 fragment %}content{% endcache %}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_cache_parse_with_vary_on() {
        let result = parse_template("{% cache 300 fragment user.id %}content{% endcache %}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_cache_too_few_args() {
        let result = parse_template("{% cache 300 %}content{% endcache %}");
        assert!(result.is_err());
    }

    #[test]
    fn test_partialdef_parse_basic() {
        let result = parse_template("{% partialdef mypart %}content{% endpartialdef %}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_partialdef_parse_inline() {
        let result = parse_template("{% partialdef mypart inline %}content{% endpartialdef %}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_partial_undefined_errors_at_render_time() {
        // Parsing succeeds (forward references are allowed); the error
        // surfaces at render time when the partial name can't be found.
        let result = render("{% partial undefined_partial %}", vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn test_partialdef_inline_renders() {
        let result = render(
            "{% partialdef mypart inline %}hello{% endpartialdef %}",
            vec![],
        );
        assert_eq!(result.unwrap(), "hello");
    }

    #[test]
    fn test_partialdef_non_inline_empty() {
        let result = render("{% partialdef mypart %}hello{% endpartialdef %}", vec![]);
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_value_to_f64() {
        assert_eq!(value_to_f64(&Value::Int(42)), 42.0);
        assert_eq!(value_to_f64(&Value::Float(3.14)), 3.14);
        assert_eq!(value_to_f64(&Value::String("10".into())), 10.0);
        assert_eq!(value_to_f64(&Value::String("bad".into())), 0.0);
        assert_eq!(value_to_f64(&Value::Bool(true)), 1.0);
        assert_eq!(value_to_f64(&Value::Bool(false)), 0.0);
        assert_eq!(value_to_f64(&Value::None), 0.0);
    }
}
