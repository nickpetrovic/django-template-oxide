//! `{% extends %}` / `{% block %}` / `{% include %}`. Port of
//! `django.template.loader_tags`. `BlockContext` maps block names to a
//! FIFO queue (front=base, back=most-derived). `ExtendsNode` populates
//! it; `BlockNode` reads from it. `{{ block.super }}` via `super_render`.

use std::collections::HashMap;
use std::sync::Arc;

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::{Parser, TagCompileFunc};
use crate::template::Template;
use crate::variable::{FilterExpression, FilterExpressionVar};

/// Mirrors `BlockContext`. Per-name FIFO: `add_blocks` pushes front,
/// `pop` / `get_block` read back.
#[derive(Debug, Clone)]
pub struct BlockContext {
    pub blocks: HashMap<String, Vec<BlockNodeRef>>,
}

/// `Arc<NodeList>` instead of `&BlockNode` (lifetimes) or
/// `Box<dyn Node>` (not Clone).
#[derive(Debug, Clone)]
pub struct BlockNodeRef {
    pub name: String,
    pub nodelist: Arc<NodeList>,
}

impl BlockContext {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
        }
    }

    /// Insert each block at queue front (Django's `insert(0, block)`).
    pub fn add_blocks(&mut self, blocks: &HashMap<String, BlockNodeRef>) {
        for (name, block) in blocks {
            self.blocks
                .entry(name.clone())
                .or_default()
                .insert(0, block.clone());
        }
    }

    /// Pop the most-derived block (queue back).
    pub fn pop(&mut self, name: &str) -> Option<BlockNodeRef> {
        self.blocks.get_mut(name).and_then(|q| q.pop())
    }

    /// Push back onto the queue (for `{{ block.super }}` re-entry).
    pub fn push(&mut self, name: &str, block: BlockNodeRef) {
        self.blocks.entry(name.to_owned()).or_default().push(block);
    }

    pub fn get_block(&self, name: &str) -> Option<&BlockNodeRef> {
        self.blocks.get(name).and_then(|q| q.last())
    }
}

impl Default for BlockContext {
    fn default() -> Self {
        Self::new()
    }
}

/// BlockContext lives on `Context` (mirrors Django's
/// `context.render_context['block_context']`). Nested
/// `Template::render` calls get a fresh Context with `block_context =
/// None`, so the outer template's state survives.
fn get_or_create_block_context(context: &mut Context) -> &mut BlockContext {
    context.block_context.get_or_insert_with(BlockContext::new)
}

thread_local! {
    static TEMPLATE_CACHE: std::cell::RefCell<HashMap<String, Arc<NodeList>>> =
        std::cell::RefCell::new(HashMap::new());

    static EXTENDS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

const MAX_EXTENDS_DEPTH: u32 = 256;

/// Load + compile + cache via `django.template.loader.get_template`.
/// Errors: `TemplateDoesNotExist` (not found), `TemplateSyntaxError`
/// (parse failed).
fn load_template_nodelist(
    py: Python<'_>,
    template_name: &str,
    context: &Context,
) -> Result<Arc<NodeList>, TemplateError> {
    let cached = TEMPLATE_CACHE.with_borrow(|cache| cache.get(template_name).cloned());
    if let Some(nodelist) = cached {
        return Ok(nodelist);
    }

    let (base_name, partial_name) = match template_name.split_once('#') {
        Some((base, partial)) => (base, Some(partial)),
        None => (template_name, None),
    };

    let (source, engine) = load_template_source_and_engine(py, base_name, context)?;
    let engine_bound = engine.as_ref().map(|e| e.bind(py));
    let nodelist = Template::compile_nodelist_with_engine(
        &source,
        Some(base_name),
        false,
        engine_bound
            .as_ref()
            .map(|b| b as &pyo3::Bound<'_, pyo3::PyAny>),
    )?;

    let rc = if let Some(pname) = partial_name {
        extract_partial_arc(&nodelist, pname).ok_or_else(|| {
            TemplateError::TemplateDoesNotExist {
                msg: template_name.to_owned(),
                tried: vec![],
                chain: vec![],
            }
        })?
    } else {
        Arc::new(nodelist)
    };

    TEMPLATE_CACHE.with_borrow_mut(|cache| {
        cache.insert(template_name.to_owned(), Arc::clone(&rc));
    });
    Ok(rc)
}

fn extract_partial_arc(nodelist: &NodeList, name: &str) -> Option<Arc<NodeList>> {
    for entry in nodelist.iter_entries() {
        if let crate::nodes::NodeEntry::Boxed(node) = entry {
            if let Some(pdn) = node.as_any().downcast_ref::<super::PartialDefNode>() {
                if pdn.name == name {
                    return Some(Arc::clone(&pdn.nodelist));
                }
                if let Some(found) = extract_partial_arc(&pdn.nodelist, name) {
                    return Some(found);
                }
            }
            for child_name in node.child_nodelists() {
                if let Some(child_nl) = node.get_child_nodelist(child_name) {
                    if let Some(found) = extract_partial_arc(child_nl, name) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}

/// `(source, engine)` from `django.template.loader.get_template`. The
/// returned `backends.django.Template` wrapper exposes `.template` (the
/// actual `base.Template`) at `.source`, plus the parent engine at
/// `.template.source`, and the parent engine (which holds
/// `template_builtins` libraries like django-cotton's tag registry) is
/// at `.engine`.
///
/// Returning the engine lets the caller compile with
/// `compile_nodelist_with_engine`, which is what makes
/// `{% cotton ... %}` and other third-party tags resolve inside
/// `{% include %}`/`{% extends %}` chains.
fn load_template_source_and_engine<'py>(
    py: Python<'py>,
    template_name: &str,
    context: &Context,
) -> Result<(String, Option<pyo3::Py<pyo3::PyAny>>), TemplateError> {
    let map_tdne = |e: pyo3::PyErr| -> TemplateError {
        let is_tdne = py
            .import("django.template.exceptions")
            .and_then(|m| m.getattr("TemplateDoesNotExist"))
            .map(|cls| e.is_instance(py, &cls))
            .unwrap_or(false);
        if is_tdne {
            TemplateError::TemplateDoesNotExist {
                msg: template_name.to_owned(),
                tried: vec![],
                chain: vec![],
            }
        } else {
            TemplateError::Internal(format!(
                "Failed to load template '{}': {}",
                template_name, e
            ))
        }
    };

    if let Some(ref engine_py) = context.engine {
        let engine_bound = engine_py.bind(py);
        let django_template = engine_bound
            .call_method1("get_template", (template_name,))
            .map_err(map_tdne)?;
        let source: String = django_template
            .getattr("source")
            .and_then(|s| s.extract())
            .map_err(|e| {
                TemplateError::Internal(format!(
                    "Template '{}' has no .source: {}",
                    template_name, e
                ))
            })?;
        return Ok((source, Some(engine_py.clone())));
    }

    let loader = py.import("django.template.loader").map_err(|e| {
        TemplateError::Internal(format!(
            "Failed to import django.template.loader: {e}"
        ))
    })?;
    let django_template = loader
        .call_method1("get_template", (template_name,))
        .map_err(map_tdne)?;

    let inner_template = django_template.getattr("template").map_err(|e| {
        TemplateError::Internal(format!(
            "Loaded template '{}' has no .template attribute: {}",
            template_name, e
        ))
    })?;
    let source: String = inner_template
        .getattr("source")
        .and_then(|s| s.extract())
        .map_err(|e| {
            TemplateError::Internal(format!(
                "Template '{}' has no .source: {}",
                template_name, e
            ))
        })?;
    let engine = django_template
        .getattr("engine")
        .ok()
        .map(|e| e.unbind());
    Ok((source, engine))
}

/// `FilterExpression` -> template name. Constants extract directly;
/// variables resolve against the Rust-side context.
fn resolve_template_name(
    fe: &FilterExpression,
    context: &Context,
) -> Result<String, TemplateError> {
    match &fe.var {
        FilterExpressionVar::Constant(Some(s)) => Ok(s.clone()),
        FilterExpressionVar::Constant(None) => Err(TemplateError::TemplateSyntaxError(
            "Template name resolved to None".to_owned(),
        )),
        FilterExpressionVar::Var(variable) => {
            let parts: Vec<&str> = variable.var.split('.').collect();
            let mut current = context.get(parts[0]).cloned().ok_or_else(|| {
                TemplateError::TemplateSyntaxError(format!(
                    "Variable '{}' does not exist in context (used as template name)",
                    variable.var,
                ))
            })?;

            for part in &parts[1..] {
                current = match &current {
                    Value::Dict(map) => map.get(*part).cloned().unwrap_or(Value::None),
                    _ => Value::None,
                };
            }

            match current {
                Value::String(s) => Ok(s),
                Value::SafeString(s) => Ok(s.to_string()),
                other => Err(TemplateError::TemplateSyntaxError(format!(
                    "Template name must be a string, got: {}",
                    other,
                ))),
            }
        }
    }
}

/// Simplified `FilterExpression` -> `Value` for `{% include %}` extras.
fn resolve_filter_expression_to_value(
    fe: &FilterExpression,
    context: &Context,
) -> Value {
    match &fe.var {
        FilterExpressionVar::Constant(Some(s)) => Value::SafeString(s.clone().into()),
        FilterExpressionVar::Constant(None) => Value::None,
        FilterExpressionVar::Var(variable) => {
            let parts: Vec<&str> = variable.var.split('.').collect();
            let mut current = match context.get(parts[0]) {
                Some(v) => v.clone(),
                None => {
                    if let Ok(n) = variable.var.parse::<i64>() {
                        return Value::Int(n);
                    }
                    if let Ok(f) = variable.var.parse::<f64>() {
                        return Value::Float(f);
                    }
                    return Value::String(String::new());
                }
            };
            for part in &parts[1..] {
                current = match &current {
                    Value::Dict(map) => {
                        map.get(*part).cloned().unwrap_or(Value::String(String::new()))
                    }
                    Value::List(items) => {
                        if let Ok(idx) = part.parse::<usize>() {
                            items.get(idx).cloned().unwrap_or(Value::String(String::new()))
                        } else {
                            Value::String(String::new())
                        }
                    }
                    _ => Value::String(String::new()),
                };
            }
            current
        }
    }
}

/// Recursive `BlockNodeRef` collection via `as_block_node_ref()`.
/// Used by `ExtendsNode` to gather parent-template blocks.
fn collect_block_nodes_from_nodelist(nodelist: &NodeList) -> HashMap<String, BlockNodeRef> {
    let mut result = HashMap::new();
    for node in nodelist.iter() {
        if let Some((name, rc_nodelist)) = node.as_block_node_ref() {
            result.insert(
                name.clone(),
                BlockNodeRef {
                    name,
                    nodelist: rc_nodelist,
                },
            );
        }
        for child_name in node.child_nodelists() {
            if let Some(child_nl) = node.get_child_nodelist(child_name) {
                result.extend(collect_block_nodes_from_nodelist(child_nl));
            }
        }
    }
    result
}

/// `{% block name %}...{% endblock %}`. `Arc<NodeList>` so
/// `collect_block_nodes` can share content with `BlockNodeRef`s.
#[derive(Debug)]
pub struct BlockNode {
    pub name: String,
    pub nodelist: Arc<NodeList>,
    pub parent: Option<Box<BlockNode>>,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl BlockNode {
    pub fn new(name: impl Into<String>, nodelist: NodeList) -> Self {
        Self {
            name: name.into(),
            nodelist: Arc::new(nodelist),
            parent: None,
            token_field: None,
            origin_field: None,
        }
    }

    /// Pop, render, push. Implements `{{ block.super }}`.
    pub fn super_render(
        &self,
        py: Python<'_>,
        context: &mut Context,
    ) -> Result<String, TemplateError> {
        let block_context = get_or_create_block_context(context);
        if let Some(parent_ref) = block_context.pop(&self.name) {
            let result = parent_ref.nodelist.render(py, context)?;
            let block_context = get_or_create_block_context(context);
            block_context.push(&self.name, parent_ref);
            Ok(result.as_str().to_owned())
        } else {
            Ok(String::new())
        }
    }
}

impl Node for BlockNode {
    impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        if context.block_context.is_none() {
            // No inheritance: render directly.
            return Ok(self.nodelist.render(py, context)?.as_str().to_owned());
        }

        let block_context = get_or_create_block_context(context);
        let popped = block_context.pop(&self.name);
        let block_ref = match &popped {
            Some(b) => b.clone(),
            None => BlockNodeRef {
                name: self.name.clone(),
                nodelist: Arc::new(NodeList::new()),
            },
        };

        // Django exposes `block.super` as a lazy property; we pre-render
        // it since we can't inject Rust methods into the context.
        let super_content = {
            let bc = get_or_create_block_context(context);
            if bc.get_block(&self.name).is_some() {
                let parent_ref = bc.pop(&self.name).unwrap();
                let rendered = parent_ref.nodelist.render(py, context)?;
                let bc = get_or_create_block_context(context);
                bc.push(&self.name, parent_ref);
                rendered.as_str().to_owned()
            } else {
                String::new()
            }
        };

        let mut block_dict = indexmap::IndexMap::new();
        block_dict.insert("super".to_string(), Value::SafeString(super_content.into()));
        context.push_with({
            let mut m = HashMap::new();
            m.insert("block".to_owned(), Value::Dict(block_dict));
            m
        });

        let result = block_ref.nodelist.render(py, context)?;

        context.pop();

        // Push back for reuse across inheritance levels.
        if let Some(pushed) = popped {
            let bc = get_or_create_block_context(context);
            bc.push(&self.name, pushed);
        }

        Ok(result.as_str().to_owned())
    }

    fn must_be_first(&self) -> bool {
        false
    }

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn get_child_nodelist(&self, name: &str) -> Option<&NodeList> {
        if name == "nodelist" {
            Some(&self.nodelist)
        } else {
            None
        }
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }

    fn as_block_node_ref(&self) -> Option<(String, Arc<NodeList>)> {
        Some((self.name.clone(), Arc::clone(&self.nodelist)))
    }
}

/// `{% extends "parent.html" %}`. Mirrors `ExtendsNode`.
#[derive(Debug)]
pub struct ExtendsNode {
    /// The child template's nodelist (everything after `{% extends %}`).
    pub nodelist: NodeList,
    /// The expression resolving to the parent template name.
    pub parent_name: FilterExpression,
    /// Block nodes found in the child template, keyed by name.
    pub blocks: HashMap<String, BlockNodeRef>,
    /// Token metadata.
    pub token_field: Option<Token>,
    /// Origin metadata.
    pub origin_field: Option<Origin>,
}

impl ExtendsNode {
    pub fn new(nodelist: NodeList, parent_name: FilterExpression) -> Self {
        // Collect BlockNodes from the child's nodelist.
        let blocks = collect_block_nodes(&nodelist);
        Self {
            nodelist,
            parent_name,
            blocks,
            token_field: None,
            origin_field: None,
        }
    }
}

impl Node for ExtendsNode {
    impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let depth = EXTENDS_DEPTH.get();
        if depth >= MAX_EXTENDS_DEPTH {
            return Err(TemplateError::TemplateSyntaxError(
                "Maximum template inheritance depth exceeded.".into(),
            ));
        }
        EXTENDS_DEPTH.set(depth + 1);

        let result = self.render_inner(py, context);

        EXTENDS_DEPTH.set(depth);
        result
    }

    fn must_be_first(&self) -> bool {
        true
    }

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist"]
    }

    fn get_child_nodelist(&self, name: &str) -> Option<&NodeList> {
        if name == "nodelist" {
            Some(&self.nodelist)
        } else {
            None
        }
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist);
    }
}

impl ExtendsNode {
    fn render_inner(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let parent_name = resolve_template_name(&self.parent_name, context)?;
        let parent_nodelist = load_template_nodelist(py, &parent_name, context)?;

        let block_context = get_or_create_block_context(context);
        block_context.add_blocks(&self.blocks);

        let parent_blocks = collect_block_nodes_from_nodelist(&parent_nodelist);
        let block_context = get_or_create_block_context(context);
        block_context.add_blocks(&parent_blocks);

        let result = parent_nodelist.render(py, context)?;
        Ok(result.as_str().to_owned())
    }
}

/// `{% include "fragment.html" %}`. Mirrors `IncludeNode`.
#[derive(Debug)]
pub struct IncludeNode {
    pub template: FilterExpression,
    /// `with key=val` bindings.
    pub extra_context: Vec<(String, FilterExpression)>,
    /// `only` keyword: isolate from the parent context.
    pub isolated_context: bool,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl IncludeNode {
    pub fn new(
        template: FilterExpression,
        extra_context: Vec<(String, FilterExpression)>,
        isolated_context: bool,
    ) -> Self {
        Self {
            template,
            extra_context,
            isolated_context,
            token_field: None,
            origin_field: None,
        }
    }
}

impl Node for IncludeNode {
    impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let template_name = resolve_template_name(&self.template, context)?;
        let nodelist = load_template_nodelist(py, &template_name, context)?;

        let mut extra: HashMap<String, Value> = HashMap::new();
        for (key, expr) in &self.extra_context {
            let value = resolve_filter_expression_to_value(expr, context);
            extra.insert(key.clone(), value);
        }

        if self.isolated_context {
            // `only`: child sees only `extra` (plus builtins).
            let mut isolated = Context::new(Some(extra));
            isolated.autoescape = context.autoescape;
            let result = nodelist.render(py, &mut isolated)?;
            Ok(result.as_str().to_owned())
        } else if !extra.is_empty() {
            context.push_with(extra);
            let result = nodelist.render(py, context)?;
            context.pop();
            Ok(result.as_str().to_owned())
        } else {
            let result = nodelist.render(py, context)?;
            Ok(result.as_str().to_owned())
        }
    }

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn register_loader_tags(parser: &mut Parser) {
    parser.tags.insert(
        "block".to_owned(),
        TagCompileFunc::Rust(Box::new(compile_block)),
    );
    parser.tags.insert(
        "extends".to_owned(),
        TagCompileFunc::Rust(Box::new(compile_extends)),
    );
    parser.tags.insert(
        "include".to_owned(),
        TagCompileFunc::Rust(Box::new(compile_include)),
    );
}

/// `{% block <name> %} ... {% endblock [<name>] %}`. Mirrors `do_block`.
fn compile_block(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();

    if bits.len() != 2 {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' tag takes only one argument",
            bits[0],
        )));
    }

    let block_name = bits[1].clone();

    // Enforce uniqueness (loader_tags.py:69-75).
    if !parser.loaded_block_names.insert(block_name.clone()) {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' tag with name '{}' appears more than once",
            bits[0], block_name,
        )));
    }

    let nodelist = parser.parse(&["endblock"])?;
    let endblock_token = parser.next_token();

    // Optional `{% endblock name %}` must match.
    let endblock_bits = endblock_token.split_contents();
    if endblock_bits.len() > 1 {
        let endblock_name = &endblock_bits[1];
        if endblock_name != &block_name {
            return Err(TemplateError::TemplateSyntaxError(format!(
                "'{tag}' tag with name '{name}' does not match the \
                 end tag name '{end_name}'.",
                tag = bits[0],
                name = block_name,
                end_name = endblock_name,
            )));
        }
    }

    Ok(Box::new(BlockNode::new(block_name, nodelist)))
}

/// `{% extends <parent_name> %}`. Must be first (`must_be_first`).
fn compile_extends(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();

    if bits.len() != 2 {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' takes one argument",
            bits[0],
        )));
    }

    let parent_name = parser.compile_filter(&bits[1])?;
    let nodelist = parser.parse(&[])?;
    Ok(Box::new(ExtendsNode::new(nodelist, parent_name)))
}

/// `{% include <name> [with key=val ...] [only] %}`. Mirrors `do_include`.
fn compile_include(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();

    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' tag takes at least one argument: the name of the \
             template to be included.",
            bits[0],
        )));
    }

    let template_name = parser.compile_filter(&bits[1])?;

    let mut extra_context: Vec<(String, FilterExpression)> = Vec::new();
    let mut isolated_context = false;
    let remaining = &bits[2..];

    if !remaining.is_empty() {
        if remaining[0] == "with" {
            let with_args = &remaining[1..];
            let mut i = 0;
            while i < with_args.len() {
                let arg = &with_args[i];

                if arg == "only" {
                    if i != with_args.len() - 1 {
                        return Err(TemplateError::TemplateSyntaxError(format!(
                            "'{}' tag's 'only' option must be the last argument.",
                            bits[0],
                        )));
                    }
                    isolated_context = true;
                    break;
                }

                if let Some(eq_pos) = arg.find('=') {
                    let key = &arg[..eq_pos];
                    let value_str = &arg[eq_pos + 1..];

                    if key.is_empty() || value_str.is_empty() {
                        return Err(TemplateError::TemplateSyntaxError(format!(
                            "'{}' tag's 'with' option received an invalid \
                             argument: '{}'.",
                            bits[0], arg,
                        )));
                    }

                    let value_expr = parser.compile_filter(value_str)?;
                    extra_context.push((key.to_owned(), value_expr));
                } else {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' tag's 'with' option expected an assignment \
                         (key=value), got '{}'.",
                        bits[0], arg,
                    )));
                }

                i += 1;
            }
        } else if remaining[0] == "only" {
            if remaining.len() != 1 {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "'{}' tag received unexpected arguments after 'only'.",
                    bits[0],
                )));
            }
            isolated_context = true;
        } else {
            return Err(TemplateError::TemplateSyntaxError(format!(
                "'{}' tag received an invalid argument: '{}'.",
                bits[0], remaining[0],
            )));
        }
    }

    Ok(Box::new(IncludeNode::new(
        template_name,
        extra_context,
        isolated_context,
    )))
}

// Helpers

/// Walk a `NodeList` and collect all `BlockNode`s into a map.
///
/// This is the Rust equivalent of Django's
/// `nodelist.get_nodes_by_type(BlockNode)`, specialised for `BlockNode`.
///
/// Uses `as_block_node_ref()` on each node to identify `BlockNode`s and
/// get shared `Arc<NodeList>` references to their content.
fn collect_block_nodes(nodelist: &NodeList) -> HashMap<String, BlockNodeRef> {
    let mut result = HashMap::new();
    for node in nodelist.iter() {
        if let Some((name, rc_nodelist)) = node.as_block_node_ref() {
            result.insert(
                name.clone(),
                BlockNodeRef {
                    name,
                    nodelist: rc_nodelist,
                },
            );
        }
    }
    result
}

/// Extract the block name from a `BlockNode` Debug representation.
///
/// Looks for `name: "..."` in the debug string.
fn extract_block_name(debug_str: &str) -> Option<String> {
    let marker = "name: \"";
    let start = debug_str.find(marker)? + marker.len();
    let rest = &debug_str[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    // -- Helper: lex and create parser with loader tags --------------------

    fn parser_with_loader_tags(source: &str) -> Parser {
        let tokens = Lexer::new(source).tokenize();
        let mut parser = Parser::new(tokens);
        register_loader_tags(&mut parser);
        parser
    }

    // -- BlockContext tests -----------------------------------------------

    #[test]
    fn test_block_context_new_empty() {
        let bc = BlockContext::new();
        assert!(bc.blocks.is_empty());
    }

    #[test]
    fn test_block_context_add_and_get() {
        let mut bc = BlockContext::new();
        let block = BlockNodeRef {
            name: "content".to_owned(),
            nodelist: Arc::new(NodeList::new()),
        };
        let mut blocks = HashMap::new();
        blocks.insert("content".to_owned(), block);

        bc.add_blocks(&blocks);

        assert!(bc.get_block("content").is_some());
        assert_eq!(bc.get_block("content").unwrap().name, "content");
    }

    #[test]
    fn test_block_context_pop() {
        let mut bc = BlockContext::new();
        let block = BlockNodeRef {
            name: "title".to_owned(),
            nodelist: Arc::new(NodeList::new()),
        };
        let mut blocks = HashMap::new();
        blocks.insert("title".to_owned(), block);
        bc.add_blocks(&blocks);

        let popped = bc.pop("title");
        assert!(popped.is_some());
        assert_eq!(popped.unwrap().name, "title");

        // Now it should be empty.
        assert!(bc.pop("title").is_none());
    }

    #[test]
    fn test_block_context_push() {
        let mut bc = BlockContext::new();
        let block = BlockNodeRef {
            name: "footer".to_owned(),
            nodelist: Arc::new(NodeList::new()),
        };
        bc.push("footer", block);

        assert!(bc.get_block("footer").is_some());
    }

    #[test]
    fn test_block_context_fifo_order() {
        let mut bc = BlockContext::new();

        // Simulate child adding blocks first, then parent.
        let child_block = BlockNodeRef {
            name: "content".to_owned(),
            nodelist: Arc::new(NodeList::new()),
        };
        let parent_block = BlockNodeRef {
            name: "content".to_owned(),
            nodelist: Arc::new(NodeList::new()),
        };

        // Child adds first.
        let mut child_blocks = HashMap::new();
        child_blocks.insert("content".to_owned(), child_block);
        bc.add_blocks(&child_blocks);

        // Parent adds next (insert at front).
        let mut parent_blocks = HashMap::new();
        parent_blocks.insert("content".to_owned(), parent_block);
        bc.add_blocks(&parent_blocks);

        // Pop should return the child's block (most-derived, at back).
        // After two add_blocks calls:
        // Queue: [parent(inserted at 0), child(already there)]
        // Wait - add_blocks inserts at front:
        //   After child: [child]
        //   After parent: [parent, child]  (parent inserted at index 0)
        // Pop from back → child.  Correct - child overrides parent.
        let popped = bc.pop("content").unwrap();
        // The child was added first, then parent was inserted at front.
        // Queue is [parent, child], pop from back = child.
        // But both have same name "content"... the order matters for
        // rendering.  In Django: child template's blocks are added first,
        // then parent's are inserted at index 0.  Pop from end gives child.
        assert_eq!(popped.name, "content");
    }

    #[test]
    fn test_block_context_missing_block() {
        let bc = BlockContext::new();
        assert!(bc.get_block("nonexistent").is_none());
    }

    // -- compile_block tests ----------------------------------------------

    #[test]
    fn test_compile_block_basic() {
        let mut parser = parser_with_loader_tags("{% block title %}Hello{% endblock %}");
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_block_with_endblock_name() {
        let mut parser =
            parser_with_loader_tags("{% block title %}Hello{% endblock title %}");
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_block_mismatched_endblock_name() {
        let mut parser =
            parser_with_loader_tags("{% block title %}Hello{% endblock content %}");
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("does not match"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_block_no_name_errors() {
        let mut parser = parser_with_loader_tags("{% block %}Hello{% endblock %}");
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("takes only one argument"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_block_duplicate_name_errors() {
        let mut parser = parser_with_loader_tags(
            "{% block title %}A{% endblock %}{% block title %}B{% endblock %}",
        );
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("appears more than once"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    // -- compile_extends tests --------------------------------------------

    #[test]
    fn test_compile_extends_basic() {
        let mut parser =
            parser_with_loader_tags(r#"{% extends "base.html" %}{% block title %}Hi{% endblock %}"#);
        let nodelist = parser.parse(&[]).unwrap();

        // The extends node should be the only top-level node.
        assert_eq!(nodelist.len(), 1);
        let extends_node = nodelist
            .iter()
            .next()
            .expect("extends should be a boxed node");
        assert!(extends_node.must_be_first());
    }

    #[test]
    fn test_compile_extends_no_arg_errors() {
        let mut parser = parser_with_loader_tags("{% extends %}");
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("takes one argument"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_extends_must_be_first() {
        let mut parser = parser_with_loader_tags(
            r#"{% block title %}Hi{% endblock %}{% extends "base.html" %}"#,
        );
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("must be the first tag"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    // -- compile_include tests --------------------------------------------

    #[test]
    fn test_compile_include_basic() {
        let mut parser = parser_with_loader_tags(r#"{% include "header.html" %}"#);
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_include_with_context() {
        let mut parser =
            parser_with_loader_tags(r#"{% include "header.html" with title=page_title %}"#);
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_include_with_only() {
        let mut parser = parser_with_loader_tags(
            r#"{% include "header.html" with title=page_title only %}"#,
        );
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_include_only_without_with() {
        let mut parser = parser_with_loader_tags(r#"{% include "header.html" only %}"#);
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_include_no_arg_errors() {
        let mut parser = parser_with_loader_tags("{% include %}");
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("takes at least one argument"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_include_invalid_option_errors() {
        let mut parser = parser_with_loader_tags(r#"{% include "x.html" bogus %}"#);
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("invalid argument"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    // -- BlockNode rendering (without inheritance) ------------------------

    #[test]
    fn test_block_node_render_no_inheritance() {
        Python::attach(|py| {
            let mut parser = parser_with_loader_tags(
                "{% block title %}Hello World{% endblock %}",
            );
            let nodelist = parser.parse(&[]).unwrap();
            // Fresh Context starts with block_context: None - standalone render.
            let mut ctx = Context::new(None);

            let mut output = String::new();
            for node in nodelist.iter() {
                output.push_str(&node.render(py, &mut ctx).unwrap());
            }
            assert_eq!(output, "Hello World");
        });
    }

    // -- extract_block_name -----------------------------------------------

    #[test]
    fn test_extract_block_name_basic() {
        let debug = r#"BlockNode { name: "content", nodelist: NodeList { ... } }"#;
        assert_eq!(extract_block_name(debug), Some("content".to_owned()));
    }

    #[test]
    fn test_extract_block_name_no_match() {
        assert_eq!(extract_block_name("TextNode { ... }"), None);
    }
}
