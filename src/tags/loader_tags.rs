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

/// Render-context key for the current template's Python Origin. Used by
/// `ExtendsNode` to seed extends history with the correct origin when
/// templates are loaded by `{% include %}` (where `context.template`
/// still points to the outer template).
const CURRENT_ORIGIN_KEY: &str = "__oxide_current_origin";

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

/// Load + compile via `django.template.loader.get_template`.
/// Errors: `TemplateDoesNotExist` (not found), `TemplateSyntaxError`
/// (parse failed).
///
/// `cache_key`: when `Some`, the compiled nodelist is cached in the
/// thread-local `TEMPLATE_CACHE` under a composite key so the same
/// `{% include %}` node reuses its compiled template across loop
/// iterations (matching Django's `context.render_context[self]`
/// pattern). Different IncludeNode instances use different keys, so
/// each include gets its own node instances with distinct render-keys.
/// When `None` (extends path), no caching is done and a fresh nodelist
/// is returned each time.
/// Returns `(nodelist, origin)`. `origin` is the Python `Origin` of the
/// loaded Django Template, `None` when served from cache or when the
/// loader path does not produce one.
fn load_template_nodelist(
    py: Python<'_>,
    template_name: &str,
    context: &Context,
    cache_key: Option<&str>,
) -> Result<(Arc<NodeList>, Option<Py<PyAny>>), TemplateError> {
    // Check per-include-node cache first.
    if let Some(key) = cache_key {
        let full_key = format!("{}\x00{}", key, template_name);
        let cached = TEMPLATE_CACHE.with_borrow(|c| c.get(&full_key).cloned());
        if let Some(nl) = cached {
            return Ok((nl, None));
        }
    }

    let (base_name, partial_name) = match template_name.split_once('#') {
        Some((base, partial)) => (base, Some(partial)),
        None => (template_name, None),
    };

    let (source, engine, origin) = load_template_source_and_engine(py, base_name, context)?;
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

    // Cache per-include-node instance.
    if let Some(key) = cache_key {
        let full_key = format!("{}\x00{}", key, template_name);
        TEMPLATE_CACHE.with_borrow_mut(|c| {
            c.insert(full_key, Arc::clone(&rc));
        });
    }

    Ok((rc, origin))
}

pub fn clear_template_cache() {
    TEMPLATE_CACHE.with_borrow_mut(|cache| cache.clear());
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
                if let Some(child_nl) = node.get_child_nodelist(child_name)
                    && let Some(found) = extract_partial_arc(child_nl, name)
                {
                    return Some(found);
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
/// Returns `(source, engine, origin)`. `origin` is the Python `Origin`
/// of the loaded Django Template, used to seed `extends_context` history
/// so same-name-multi-loader chains skip correctly.
#[allow(clippy::type_complexity)]
fn load_template_source_and_engine<'py>(
    py: Python<'py>,
    template_name: &str,
    context: &Context,
) -> Result<
    (
        String,
        Option<pyo3::Py<pyo3::PyAny>>,
        Option<pyo3::Py<pyo3::PyAny>>,
    ),
    TemplateError,
> {
    let map_py_err = |e: pyo3::PyErr| -> TemplateError {
        let exc_mod = py.import("django.template.exceptions");
        let is_tdne = exc_mod
            .as_ref()
            .ok()
            .and_then(|m| m.getattr("TemplateDoesNotExist").ok())
            .map(|cls| e.is_instance(py, &cls))
            .unwrap_or(false);
        if is_tdne {
            return TemplateError::TemplateDoesNotExist {
                msg: template_name.to_owned(),
                tried: vec![],
                chain: vec![],
            };
        }
        let is_tse = exc_mod
            .as_ref()
            .ok()
            .and_then(|m| m.getattr("TemplateSyntaxError").ok())
            .map(|cls| e.is_instance(py, &cls))
            .unwrap_or(false);
        if is_tse {
            let msg = e
                .value(py)
                .str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| format!("{}", e));
            return TemplateError::TemplateSyntaxError(msg);
        }
        TemplateError::Internal(format!(
            "Failed to load template '{}': {}",
            template_name, e
        ))
    };

    if let Some(ref engine_py) = context.engine {
        let engine_bound = engine_py.bind(py);
        let django_template = engine_bound
            .call_method1("get_template", (template_name,))
            .map_err(map_py_err)?;
        let source: String = django_template
            .getattr("source")
            .and_then(|s| s.extract())
            .map_err(|e| {
                TemplateError::Internal(format!(
                    "Template '{}' has no .source: {}",
                    template_name, e
                ))
            })?;
        let origin = django_template
            .getattr("origin")
            .ok()
            .and_then(|o| if o.is_none() { None } else { Some(o.unbind()) });
        return Ok((source, Some(engine_py.clone()), origin));
    }

    let loader = py.import("django.template.loader").map_err(|e| {
        TemplateError::Internal(format!("Failed to import django.template.loader: {e}"))
    })?;
    let django_template = loader
        .call_method1("get_template", (template_name,))
        .map_err(map_py_err)?;

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
    let origin = inner_template
        .getattr("origin")
        .ok()
        .and_then(|o| if o.is_none() { None } else { Some(o.unbind()) });
    let engine = django_template.getattr("engine").ok().map(|e| e.unbind());
    Ok((source, engine, origin))
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
            let current = match context.get(parts[0]) {
                Some(v) => {
                    let mut cur = v.clone();
                    for part in &parts[1..] {
                        cur = match &cur {
                            Value::Dict(map) => map.get(*part).cloned().unwrap_or(Value::None),
                            Value::List(items) => {
                                if let Ok(idx) = part.parse::<usize>() {
                                    items.get(idx).cloned().unwrap_or(Value::None)
                                } else {
                                    Value::None
                                }
                            }
                            _ => Value::None,
                        };
                    }
                    cur
                }
                None => {
                    // Variable not found: use string_if_invalid behavior
                    if context.string_if_invalid.is_empty() {
                        return Err(TemplateError::TemplateSyntaxError(format!(
                            "Variable '{}' does not exist in context (used as template name)",
                            variable.var,
                        )));
                    } else {
                        Value::String(context.string_if_invalid.clone())
                    }
                }
            };

            match current {
                Value::String(s) => Ok(s),
                Value::SafeString(s) => Ok(s.to_string()),
                Value::None => Err(TemplateError::TemplateSyntaxError(
                    "Template name resolved to None".to_owned(),
                )),
                Value::PyObject(obj) => {
                    // Try to extract as string first.
                    Python::attach(|py| {
                        let bound = obj.bind(py);
                        if let Ok(s) = bound.extract::<String>() {
                            return Ok(s);
                        }
                        let type_name = bound
                            .get_type()
                            .name()
                            .map(|n| n.to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        Err(TemplateError::TemplateSyntaxError(format!(
                            "Template name must be a string, got: {}",
                            type_name,
                        )))
                    })
                }
                other => Err(TemplateError::TemplateSyntaxError(format!(
                    "Template name must be a string, got: {}",
                    other,
                ))),
            }
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
        // Use walk_children to reach all child nodelists, including
        // IfNode branches and ForNode bodies where blocks might live.
        node.walk_children(&mut |child_nl: &NodeList| {
            result.extend(collect_block_nodes_from_nodelist(child_nl));
        });
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
            // No inheritance: check for block.super usage, which is
            // an error in a base template (matches Django behavior).
            if nodelist_uses_block_super(&self.nodelist) {
                return Err(TemplateError::TemplateSyntaxError(
                    "'BlockNode' object has no attribute 'context'. Did you use \
                     {{ block.super }} in a base template?"
                        .to_owned(),
                ));
            }
            // No inheritance: render directly.
            return Ok(self.nodelist.render(py, context)?.as_str().to_owned());
        }

        let block_context = get_or_create_block_context(context);
        let popped = block_context.pop(&self.name);
        let block_ref = match &popped {
            Some(b) => b.clone(),
            None => BlockNodeRef {
                name: self.name.clone(),
                nodelist: Arc::clone(&self.nodelist),
            },
        };

        // Recursively compute block.super: render the parent block
        // with ITS own block.super set up, so multi-level inheritance
        // chains like grandchild -> child -> parent all work.
        let super_content = render_block_super(py, context, &self.name)?;

        let mut block_dict = crate::context::ValueMap::default();
        block_dict.insert(
            compact_str::CompactString::const_new("super"),
            Value::SafeString(super_content.into()),
        );
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

/// Recursively render block.super for a named block. Pops the next
/// parent block from the block context, sets up ITS block.super via
/// recursion, renders the parent nodelist with block.super in scope,
/// then pushes the parent back. Returns the rendered content.
fn render_block_super(
    py: Python<'_>,
    context: &mut Context,
    block_name: &str,
) -> Result<String, TemplateError> {
    let bc = get_or_create_block_context(context);
    if bc.get_block(block_name).is_none() {
        return Ok(String::new());
    }
    let parent_ref = bc.pop(block_name).unwrap();

    // Recursively get this parent's own block.super
    let parent_super_content = render_block_super(py, context, block_name)?;

    // Set up block.super for the parent's render
    let mut block_dict = crate::context::ValueMap::default();
    block_dict.insert(
        compact_str::CompactString::const_new("super"),
        Value::SafeString(parent_super_content.into()),
    );
    context.push_with({
        let mut m = HashMap::new();
        m.insert("block".to_owned(), Value::Dict(block_dict));
        m
    });

    let rendered = parent_ref.nodelist.render(py, context)?;

    context.pop();

    // Push back for reuse
    let bc = get_or_create_block_context(context);
    bc.push(block_name, parent_ref);

    Ok(rendered.as_str().to_owned())
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
        // Try resolving template name; if it resolves to a Template object,
        // compile its source directly instead of loading by name.
        let parent_nodelist = self.resolve_parent_nodelist(py, context)?;

        let block_context = get_or_create_block_context(context);
        block_context.add_blocks(&self.blocks);

        // Only add parent blocks if the parent is the ROOT template
        // (has no ExtendsNode). If the parent has its own ExtendsNode,
        // that node will add blocks during its own render. This
        // prevents double-adding blocks for intermediate templates.
        // Mirrors Django's ExtendsNode.render (loader_tags.py).
        let parent_has_extends = parent_nodelist.iter().any(|node| node.must_be_first());
        if !parent_has_extends {
            let parent_blocks = collect_block_nodes_from_nodelist(&parent_nodelist);
            let block_context = get_or_create_block_context(context);
            block_context.add_blocks(&parent_blocks);
        }

        let result = parent_nodelist.render(py, context)?;
        Ok(result.as_str().to_owned())
    }

    /// Resolve the parent template. Handles:
    /// - String names (load via engine/loader)
    /// - Django Template objects (compile their source directly)
    /// - None (error)
    fn resolve_parent_nodelist(
        &self,
        py: Python<'_>,
        context: &mut Context,
    ) -> Result<Arc<NodeList>, TemplateError> {
        // Get the Python Origin of the currently-rendering template.
        // Django's `ExtendsNode.find_template` initialises history with
        // `[self.origin]` which is the parser-set origin (carrying the
        // full filesystem path and loader reference from the engine).
        //
        // Check render_context first: IncludeNode stores the loaded
        // template's origin under CURRENT_ORIGIN_KEY so inner extends
        // chains use the correct origin (not the outer template's).
        let current_origin: Option<pyo3::Py<pyo3::PyAny>> = context
            .render_context
            .get(CURRENT_ORIGIN_KEY)
            .and_then(|v| match v {
                Value::PyObject(obj) => Some(obj.clone_ref(py)),
                _ => None,
            })
            .or_else(|| {
                context.template.as_ref().and_then(|tref| {
                    let bound = tref.obj.bind(py);
                    bound
                        .getattr("origin")
                        .ok()
                        .and_then(|o| if o.is_none() { None } else { Some(o.unbind()) })
                })
            });

        // First, try to resolve as a string name
        match resolve_template_name(&self.parent_name, context) {
            Ok(name) => {
                let engine_clone = context.engine.as_ref().map(|e| e.clone_ref(py));
                if let Some(ref engine_py) = engine_clone {
                    let origin_ref = current_origin.as_ref().map(|o| o.bind(py));
                    load_template_with_history(py, &name, engine_py, context, origin_ref)
                } else {
                    load_template_nodelist(py, &name, context, None).map(|(nl, _origin)| nl)
                }
            }
            Err(_) => {
                // Try resolving as a Template object via full expression resolution
                let val = super::resolve_if_value(py, &self.parent_name, context);
                match &val {
                    Value::PyObject(obj) => {
                        let bound = obj.bind(py);
                        // Try .source (base.Template) or .template.source (backends.django.Template)
                        let source_str = bound
                            .getattr("source")
                            .and_then(|s| s.extract::<String>())
                            .or_else(|_| {
                                bound
                                    .getattr("template")
                                    .and_then(|t| t.getattr("source"))
                                    .and_then(|s| s.extract::<String>())
                            });

                        if let Ok(src) = source_str {
                            let engine_bound = context.engine.as_ref().map(|e| e.bind(py));
                            let nl = Template::compile_nodelist_with_engine(
                                &src,
                                None,
                                false,
                                engine_bound
                                    .as_ref()
                                    .map(|b| b as &pyo3::Bound<'_, pyo3::PyAny>),
                            )?;
                            Ok(Arc::new(nl))
                        } else {
                            Err(TemplateError::TemplateSyntaxError(format!(
                                "Template name must be a string or Template, got: {}",
                                bound
                                    .get_type()
                                    .name()
                                    .map(|n| n.to_string())
                                    .unwrap_or_else(|_| "unknown".to_string()),
                            )))
                        }
                    }
                    Value::None => Err(TemplateError::TemplateSyntaxError(
                        "Template name resolved to None".to_owned(),
                    )),
                    _ => Err(TemplateError::TemplateSyntaxError(format!(
                        "Template name must be a string, got: {}",
                        val,
                    ))),
                }
            }
        }
    }
}

/// Load a template using Django's `engine.find_template(name, skip=history)`.
/// Tracks extend history in `context.render_context` to support recursive
/// extends across multiple loaders (matching Django's ExtendsNode.find_template).
///
/// Django's `ExtendsNode.find_template` initialises history with
/// `[self.origin]` the first time and stores it in
/// `context.render_context["extends_context"]`.  Successive extends
/// calls within the same render share the same list, preventing the
/// engine from returning the same template twice.
///
/// `current_origin` is the Python `Origin` of the template that
/// contains the `{% extends %}` tag.
fn load_template_with_history(
    py: Python<'_>,
    template_name: &str,
    engine_py: &Py<PyAny>,
    context: &mut Context,
    current_origin: Option<&pyo3::Bound<'_, pyo3::PyAny>>,
) -> Result<Arc<NodeList>, TemplateError> {
    let engine_bound = engine_py.bind(py);

    // Build the skip/history list from render_context.
    // Key matches Django's ExtendsNode.context_key = "extends_context".
    let history_key = "extends_context".to_owned();

    let history_list = match context.render_context.get(&history_key) {
        Some(Value::PyObject(obj)) => obj.clone_ref(py),
        _ => {
            // First extends: create the history list seeded with the
            // current template's origin (matches Django's
            // `context.render_context.setdefault(key, [self.origin])`).
            let list = pyo3::types::PyList::empty(py);
            if let Some(origin) = current_origin {
                list.append(origin).map_err(|e| {
                    TemplateError::Internal(format!("Failed to append origin: {e}"))
                })?;
            }
            let py_obj = list.clone().into_any().unbind();
            context
                .render_context
                .set(history_key.clone(), Value::PyObject(py_obj.clone_ref(py)));
            py_obj
        }
    };

    let history_bound = history_list.bind(py);

    // Call engine.find_template(template_name, skip=history_list)
    let find_kwargs = pyo3::types::PyDict::new(py);
    find_kwargs
        .set_item("skip", history_bound)
        .map_err(|e| TemplateError::Internal(format!("Failed to set skip: {e}")))?;

    let find_result =
        engine_bound.call_method("find_template", (template_name,), Some(&find_kwargs));

    let (django_template, origin) = match find_result {
        Ok(result) => {
            // Returns (Template, Origin)
            let template = result
                .get_item(0)
                .map_err(|e| TemplateError::Internal(format!("find_template result error: {e}")))?;
            let origin = result
                .get_item(1)
                .map_err(|e| TemplateError::Internal(format!("find_template origin error: {e}")))?;
            (template, origin)
        }
        Err(e) => {
            // Check if TemplateDoesNotExist - propagate the Python
            // exception directly so `.tried` is preserved.
            let exc_mod = py.import("django.template.exceptions");
            let is_tdne = exc_mod
                .as_ref()
                .ok()
                .and_then(|m| m.getattr("TemplateDoesNotExist").ok())
                .map(|cls| e.is_instance(py, &cls))
                .unwrap_or(false);
            if is_tdne {
                return Err(TemplateError::PythonError(e));
            }
            return Err(TemplateError::PythonError(e));
        }
    };

    // Add origin to history
    let history_list_ref = history_bound
        .cast::<pyo3::types::PyList>()
        .map_err(|_| TemplateError::Internal("History is not a list".into()))?;
    history_list_ref
        .append(&origin)
        .map_err(|e| TemplateError::Internal(format!("Failed to append to history: {e}")))?;

    let (base_name, partial_name) = match template_name.split_once('#') {
        Some((base, partial)) => (base, Some(partial)),
        None => (template_name, None),
    };

    let cache_key = if partial_name.is_none() {
        origin
            .getattr("name")
            .ok()
            .and_then(|n| n.extract::<String>().ok())
            .map(|o| format!("extends://{o}"))
    } else {
        None
    };

    if let Some(ref key) = cache_key
        && let Some(nl) = TEMPLATE_CACHE.with_borrow(|c| c.get(key).cloned())
    {
        return Ok(nl);
    }

    let source: String = django_template
        .getattr("source")
        .and_then(|s| s.extract())
        .map_err(|e| {
            TemplateError::Internal(format!(
                "Template '{}' has no .source: {}",
                template_name, e
            ))
        })?;

    let engine_bound2 = engine_py.bind(py);
    let nodelist = Template::compile_nodelist_with_engine(
        &source,
        Some(base_name),
        false,
        Some(engine_bound2),
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

    if let Some(key) = cache_key {
        TEMPLATE_CACHE.with_borrow_mut(|c| {
            c.insert(key, Arc::clone(&rc));
        });
    }

    Ok(rc)
}

static INCLUDE_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `{% include "fragment.html" %}`. Mirrors `IncludeNode`.
#[derive(Debug)]
pub struct IncludeNode {
    pub template: FilterExpression,
    /// `with key=val` bindings.
    pub extra_context: Vec<(String, FilterExpression)>,
    /// `only` keyword: isolate from the parent context.
    pub isolated_context: bool,
    /// Unique ID for per-include template caching.
    pub cache_key: String,
    pub token_field: Option<Token>,
    pub origin_field: Option<Origin>,
}

impl IncludeNode {
    pub fn new(
        template: FilterExpression,
        extra_context: Vec<(String, FilterExpression)>,
        isolated_context: bool,
    ) -> Self {
        let id = INCLUDE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            template,
            extra_context,
            isolated_context,
            cache_key: format!("__inc_{}", id),
            token_field: None,
            origin_field: None,
        }
    }
}

impl Node for IncludeNode {
    impl_node_metadata!();

    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        // Resolve the template expression; it could be a string name,
        // a Django Template object, None, or an iterable of template names.
        let template_val = super::resolve_if_value(py, &self.template, context);

        // For variable template names, resolve relative paths at
        // runtime, matching Django's IncludeNode.render which calls
        // `construct_relative_path(self.origin.template_name, name)`.
        let resolve_name = |name: &str| -> String {
            if name.starts_with("./") || name.starts_with("../") {
                let current_template = self
                    .origin_field
                    .as_ref()
                    .and_then(|o| o.template_name.as_deref());
                if let Some(resolved) = construct_relative_path(current_template, name) {
                    return resolved;
                }
            }
            name.to_owned()
        };

        let nodelist;
        let mut loaded_origin: Option<Py<PyAny>> = None;
        match &template_val {
            Value::String(s) if s.is_empty() => {
                // Variable not found -> TemplateSyntaxError
                return Err(TemplateError::TemplateSyntaxError(
                    "Template name resolved to empty string".to_owned(),
                ));
            }
            Value::String(s) => {
                let resolved = resolve_name(s);
                let (nl, origin) =
                    load_template_nodelist(py, &resolved, context, Some(&self.cache_key))?;
                nodelist = nl;
                loaded_origin = origin;
            }
            Value::SafeString(s) => {
                let resolved = resolve_name(s.as_ref());
                let (nl, origin) =
                    load_template_nodelist(py, &resolved, context, Some(&self.cache_key))?;
                nodelist = nl;
                loaded_origin = origin;
            }
            Value::None => {
                return Err(TemplateError::TemplateDoesNotExist {
                    msg: "No template names provided".to_owned(),
                    tried: vec![],
                    chain: vec![],
                });
            }
            Value::PyObject(obj) => {
                let bound = obj.bind(py);

                // Try to get source: directly (.source) or via wrapper (.template.source)
                let source_str = bound
                    .getattr("source")
                    .and_then(|s| s.extract::<String>())
                    .or_else(|_| {
                        bound
                            .getattr("template")
                            .and_then(|t| t.getattr("source"))
                            .and_then(|s| s.extract::<String>())
                    });

                if let Ok(src) = source_str {
                    // It's a Django Template object - compile its source
                    let engine_bound = context.engine.as_ref().map(|e| e.bind(py));
                    let nl = Template::compile_nodelist_with_engine(
                        &src,
                        None,
                        false,
                        engine_bound
                            .as_ref()
                            .map(|b| b as &pyo3::Bound<'_, pyo3::PyAny>),
                    )?;
                    nodelist = Arc::new(nl);
                } else if let Ok(name) = bound.extract::<String>() {
                    let (nl, origin) =
                        load_template_nodelist(py, &name, context, Some(&self.cache_key))?;
                    nodelist = nl;
                    loaded_origin = origin;
                } else if bound.is_none() {
                    return Err(TemplateError::TemplateDoesNotExist {
                        msg: "No template names provided".to_owned(),
                        tried: vec![],
                        chain: vec![],
                    });
                } else {
                    // Try iterating as a list of template names
                    if let Ok(iter) = bound.try_iter() {
                        let mut last_err = None;
                        for item in iter.flatten() {
                            let name = if let Ok(s) = item.extract::<String>() {
                                s
                            } else if let Ok(source) = item.getattr("source") {
                                if let Ok(src) = source.extract::<String>() {
                                    let engine_bound = context.engine.as_ref().map(|e| e.bind(py));
                                    match Template::compile_nodelist_with_engine(
                                        &src,
                                        None,
                                        false,
                                        engine_bound
                                            .as_ref()
                                            .map(|b| b as &pyo3::Bound<'_, pyo3::PyAny>),
                                    ) {
                                        Ok(_) => {
                                            last_err = None;
                                            break;
                                        }
                                        Err(e) => {
                                            last_err = Some(e);
                                            continue;
                                        }
                                    }
                                } else {
                                    continue;
                                }
                            } else {
                                continue;
                            };
                            match load_template_nodelist(py, &name, context, Some(&self.cache_key))
                            {
                                Ok((nl, _origin)) => {
                                    // Found! Use this directly with extra context below
                                    let mut extra: HashMap<String, Value> = HashMap::new();
                                    for (key, expr) in &self.extra_context {
                                        let value = super::resolve_if_value(py, expr, context);
                                        extra.insert(key.clone(), value);
                                    }
                                    if self.isolated_context {
                                        let mut isolated = Context::new(Some(extra));
                                        isolated.autoescape = context.autoescape;
                                        isolated.use_l10n = context.use_l10n;
                                        isolated.use_tz = context.use_tz;
                                        isolated.string_if_invalid =
                                            context.string_if_invalid.clone();
                                        isolated.engine = context.engine.clone();
                                        isolated.debug = context.debug;
                                        let result = nl.render(py, &mut isolated)?;
                                        return Ok(result.as_str().to_owned());
                                    } else if !extra.is_empty() {
                                        context.push_with(extra);
                                        let result = nl.render(py, context)?;
                                        context.pop();
                                        return Ok(result.as_str().to_owned());
                                    } else {
                                        let result = nl.render(py, context)?;
                                        return Ok(result.as_str().to_owned());
                                    }
                                }
                                Err(e) => {
                                    last_err = Some(e);
                                    continue;
                                }
                            }
                        }
                        if let Some(e) = last_err {
                            return Err(e);
                        }
                        return Err(TemplateError::TemplateDoesNotExist {
                            msg: "No template names provided".to_owned(),
                            tried: vec![],
                            chain: vec![],
                        });
                    } else {
                        return Err(TemplateError::TemplateSyntaxError(format!(
                            "Template name must be a string, got: {}",
                            bound
                                .get_type()
                                .name()
                                .map(|n| n.to_string())
                                .unwrap_or_else(|_| "unknown".to_string()),
                        )));
                    }
                }
            }
            other => {
                return Err(TemplateError::TemplateSyntaxError(format!(
                    "Template name must be a string, got: {}",
                    other,
                )));
            }
        }

        let mut extra: HashMap<String, Value> = HashMap::new();
        for (key, expr) in &self.extra_context {
            // Use the full expression resolver that applies filters
            let value = super::resolve_if_value(py, expr, context);
            extra.insert(key.clone(), value);
        }

        let saved_block_context = context.block_context.take();

        // Save and clear extends_context and current_origin so the
        // included template's extends chain starts fresh (matching
        // Django's Template.render which wraps in push_state). We
        // don't push a full render_context layer because {% ifchanged %}
        // state must persist across includes within a for loop.
        let saved_extends_ctx = context.render_context.get("extends_context").cloned();
        let saved_current_origin = context.render_context.get(CURRENT_ORIGIN_KEY).cloned();
        // Remove extends_context from current layer so inner extends
        // chain starts fresh.
        if saved_extends_ctx.is_some() {
            context
                .render_context
                .set("extends_context".to_owned(), Value::None);
        }

        // Store the loaded template's Python Origin so ExtendsNode
        // inside the included template can use it to seed extends
        // history correctly (instead of context.template.origin which
        // points to the outer template).
        if let Some(origin) = loaded_origin {
            context
                .render_context
                .set(CURRENT_ORIGIN_KEY.to_owned(), Value::PyObject(origin));
        }

        let render_result = if self.isolated_context {
            let mut isolated = Context::new(Some(extra));
            isolated.autoescape = context.autoescape;
            isolated.use_l10n = context.use_l10n;
            isolated.use_tz = context.use_tz;
            isolated.string_if_invalid = context.string_if_invalid.clone();
            isolated.engine = context.engine.clone();
            isolated.debug = context.debug;
            nodelist.render(py, &mut isolated)
        } else if !extra.is_empty() {
            context.push_with(extra);
            let r = nodelist.render(py, context);
            context.pop();
            r
        } else {
            nodelist.render(py, context)
        };

        // Restore extends_context and current_origin.
        if let Some(v) = saved_extends_ctx {
            context.render_context.set("extends_context".to_owned(), v);
        } else {
            // Remove the key the inner chain may have created.
            context
                .render_context
                .set("extends_context".to_owned(), Value::None);
        }
        if let Some(v) = saved_current_origin {
            context.render_context.set(CURRENT_ORIGIN_KEY.to_owned(), v);
        } else {
            context
                .render_context
                .set(CURRENT_ORIGIN_KEY.to_owned(), Value::None);
        }

        context.block_context = saved_block_context;

        Ok(render_result?.as_str().to_owned())
    }

    fn child_nodelists(&self) -> &[&str] {
        &[]
    }
}

pub fn register_loader_tags(parser: &mut Parser) {
    parser.tags.insert(
        "block".to_owned(),
        TagCompileFunc::Rust(std::rc::Rc::new(compile_block)),
    );
    parser.tags.insert(
        "extends".to_owned(),
        TagCompileFunc::Rust(std::rc::Rc::new(compile_extends)),
    );
    parser.tags.insert(
        "include".to_owned(),
        TagCompileFunc::Rust(std::rc::Rc::new(compile_include)),
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

    let resolved = maybe_resolve_relative(parser, &bits[1]);
    let parent_name = parser.compile_filter(&resolved)?;
    let nodelist = parser.parse(&[])?;
    Ok(Box::new(ExtendsNode::new(nodelist, parent_name)))
}

fn construct_relative_path(current_template: Option<&str>, relative: &str) -> Option<String> {
    if !relative.starts_with("./") && !relative.starts_with("../") {
        return None;
    }
    let current = current_template?;
    let dir = match current.rfind('/') {
        Some(pos) => &current[..pos],
        None => "",
    };
    let joined = if dir.is_empty() {
        relative.to_owned()
    } else {
        format!("{}/{}", dir, relative)
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                if parts.is_empty() {
                    return None;
                }
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn maybe_resolve_relative(parser: &Parser, name: &str) -> String {
    let unquoted = name.trim_matches(|c| c == '"' || c == '\'');
    if !unquoted.starts_with("./") && !unquoted.starts_with("../") {
        return name.to_owned();
    }
    let current = parser
        .origin
        .as_ref()
        .and_then(|o| o.template_name.as_deref());
    match construct_relative_path(current, unquoted) {
        Some(resolved) => {
            let q = &name[..1];
            format!("{}{}{}", q, resolved, q)
        }
        None => name.to_owned(),
    }
}

fn compile_include(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();

    if bits.len() < 2 {
        return Err(TemplateError::TemplateSyntaxError(format!(
            "'{}' tag takes at least one argument: the name of the \
             template to be included.",
            bits[0],
        )));
    }

    let resolved_name = maybe_resolve_relative(parser, &bits[1]);
    let template_name = parser.compile_filter(&resolved_name)?;

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
            isolated_context = true;
            if remaining.len() > 1 {
                // "only with key=val ..." syntax
                if remaining[1] == "with" {
                    let with_args = &remaining[2..];
                    for arg in with_args {
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
                    }
                } else {
                    return Err(TemplateError::TemplateSyntaxError(format!(
                        "'{}' tag received unexpected arguments after 'only'.",
                        bits[0],
                    )));
                }
            }
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

/// Check if a nodelist contains `{{ block.super }}` references.
fn nodelist_uses_block_super(nodelist: &NodeList) -> bool {
    for entry in nodelist.iter_entries() {
        if let crate::nodes::NodeEntry::Variable(var_node) = entry
            && let Some(token) = var_node.token()
            && token.contents.contains("block.super")
        {
            return true;
        }
    }
    false
}

/// Extract the block name from a `BlockNode` Debug representation.
///
/// Looks for `name: "..."` in the debug string.
#[cfg(test)]
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
        let mut parser = parser_with_loader_tags("{% block title %}Hello{% endblock title %}");
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_compile_block_mismatched_endblock_name() {
        let mut parser = parser_with_loader_tags("{% block title %}Hello{% endblock content %}");
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
        let mut parser = parser_with_loader_tags(
            r#"{% extends "base.html" %}{% block title %}Hi{% endblock %}"#,
        );
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
        let mut parser =
            parser_with_loader_tags(r#"{% include "header.html" with title=page_title only %}"#);
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
            let mut parser = parser_with_loader_tags("{% block title %}Hello World{% endblock %}");
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
