//! `{% for %}` / `{% empty %}` / `{% endfor %}`. Port of
//! `defaulttags.do_for` plus a `ForBatchPlan` extension: collect
//! `loopvar.path` references and resolve them via a single
//! `operator.attrgetter(*paths)(item)` per iteration.

use std::collections::HashMap;

use pyo3::prelude::*;

use crate::context::{Context, ContextDict, Value};
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::Parser;
use crate::variable::FilterExpression;

use super::resolve_if_value;
use super::IfNode;

#[derive(Debug)]
pub struct ForNode {
    /// `["x"]` or `["x", "y"]` (tuple unpacking).
    loopvars: Vec<String>,
    sequence: FilterExpression,
    is_reversed: bool,
    nodelist_loop: NodeList,
    nodelist_empty: Option<NodeList>,
    /// True iff the body references `forloop.*`. When false the render
    /// skips building/updating the `forloop` dict.
    body_uses_forloop: bool,
    /// `None` when batching wouldn't help (tuple unpacking, no
    /// loopvar refs, <=1 distinct path).
    batch_plan: Option<std::sync::Arc<ForBatchPlan>>,
    /// JIT body program. `OnceCell<Option<_>>` encodes both
    /// "not yet compiled" and "compilation failed permanently".
    body_program: once_cell::sync::OnceCell<Option<crate::body_program::BodyProgram>>,
    token_field: Option<Token>,
    origin_field: Option<Origin>,
}

/// Parse-time batch plan. `attrgetter(item)` produces one tuple of
/// pre-extracted attrs per iteration; `VariableNode` reads slot index.
#[derive(Debug)]
pub(crate) struct ForBatchPlan {
    /// `["candidate.name", "posting.title"]` for body referencing
    /// `app.candidate.name` and `app.posting.title`.
    paths: Vec<String>,
    /// Stripped-of-loopvar path -> slot index. Shared via Arc with
    /// `Context.loop_batch_cache.path_to_slot`.
    path_to_slot: std::sync::Arc<std::collections::HashMap<String, u16>>,
    /// `operator.attrgetter(*paths)` built lazily under the GIL.
    attrgetter: once_cell::sync::OnceCell<Py<pyo3::PyAny>>,
}

/// `list(map(attrgetter, iterable))` from Rust: one FFI crossing,
/// inner attribute walks happen in CPython's C loop. Returns Err on
/// AttributeError; caller falls back to per-item resolution.
fn prebatch_extract_from_iterable(
    py: pyo3::Python<'_>,
    plan: &ForBatchPlan,
    iterable: &pyo3::Bound<'_, pyo3::PyAny>,
) -> pyo3::PyResult<Vec<Py<pyo3::PyAny>>> {
    use pyo3::types::PyList;

    let getter = plan.get_attrgetter(py)?;
    let (map_fn, list_fn) = cached_map_list(py)?;

    // `list(map(getter, iterable))` in one FFI hop.
    let mapped = map_fn.bind(py).call1((getter.bind(py), iterable))?;
    let result_list = list_fn.bind(py).call1((mapped,))?;
    let result_list = result_list.cast::<PyList>().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err("expected list from list(map(...))")
    })?;

    let mut out = Vec::with_capacity(result_list.len());
    for tup in result_list.iter() {
        out.push(tup.unbind());
    }
    Ok(out)
}

/// Cached `builtins.map` / `builtins.list` to skip FFI lookups per prebatch.
fn cached_map_list(py: pyo3::Python<'_>) -> pyo3::PyResult<(&'static Py<pyo3::PyAny>, &'static Py<pyo3::PyAny>)> {
    static MAP_FN: std::sync::OnceLock<Py<pyo3::PyAny>> = std::sync::OnceLock::new();
    static LIST_FN: std::sync::OnceLock<Py<pyo3::PyAny>> = std::sync::OnceLock::new();

    if MAP_FN.get().is_none() || LIST_FN.get().is_none() {
        let builtins = py.import("builtins")?;
        let map_fn = builtins.getattr("map")?.unbind();
        let list_fn = builtins.getattr("list")?.unbind();
        let _ = MAP_FN.set(map_fn);
        let _ = LIST_FN.set(list_fn);
    }

    Ok((
        MAP_FN.get().expect("MAP_FN initialised"),
        LIST_FN.get().expect("LIST_FN initialised"),
    ))
}

impl ForBatchPlan {
    /// `None` when not worth it (no loopvar refs, ≤1 path, tuple
    /// unpacking, or non-trivial paths). Breakeven for attrgetter
    /// vs individual getattr is N=2.
    pub(crate) fn compute(loopvar: &str, body: &NodeList) -> Option<Self> {
        use std::collections::HashMap;

        let mut paths: Vec<String> = Vec::new();
        let mut path_to_slot: HashMap<String, u16> = HashMap::new();

        collect_loopvar_paths(body, loopvar, &mut paths, &mut path_to_slot);

        if paths.len() < 2 {
            return None;
        }

        Some(ForBatchPlan {
            paths,
            path_to_slot: std::sync::Arc::new(path_to_slot),
            attrgetter: once_cell::sync::OnceCell::new(),
        })
    }

    pub(crate) fn get_attrgetter(&self, py: pyo3::Python<'_>) -> pyo3::PyResult<&Py<pyo3::PyAny>> {
        self.attrgetter.get_or_try_init(|| {
            let operator = py.import("operator")?;
            let attrgetter = operator.getattr("attrgetter")?;
            // operator.attrgetter accepts the paths as varargs; build a
            // Python tuple to pass them. For N paths this is a single
            // C-level construction.
            let args = pyo3::types::PyTuple::new(py, self.paths.iter().map(|s| s.as_str()))?;
            let getter = attrgetter.call1(args)?;
            Ok(getter.unbind())
        })
    }

    pub(crate) fn path_to_slot(&self) -> &std::sync::Arc<std::collections::HashMap<String, u16>> {
        &self.path_to_slot
    }
}

/// Recursively walk a NodeList, accumulating attribute paths that
/// reference the given `loopvar`. For a body containing
/// `{{ app.candidate.name }}` with `loopvar="app"`, registers
/// `"candidate.name"` at the next free slot.
///
/// Nested loops that re-bind `loopvar` are skipped - their references
/// belong to the inner scope and would resolve incorrectly against the
/// outer cache.
fn collect_loopvar_paths(
    nodelist: &NodeList,
    loopvar: &str,
    paths: &mut Vec<String>,
    path_to_slot: &mut std::collections::HashMap<String, u16>,
) {
    use crate::nodes::NodeEntry;

    for entry in nodelist.iter_entries() {
        match entry {
            NodeEntry::Variable(vn) => {
                visit_filter_expression(&vn.filter_expression, loopvar, paths, path_to_slot);
            }
            NodeEntry::Boxed(node) => {
                // IfNode: walk only the branch bodies, NOT their
                // conditions. Conditions are evaluated by the dynamic
                // resolver - including them in the batch would force
                // every iteration to pre-resolve them, and if any
                // condition variable is missing (e.g.
                // Don't poison the batch: a missing attr on any item
                // makes attrgetter raise AttributeError, fallback
                // applies for the whole body.
                if let Some(if_node) = node.as_any().downcast_ref::<IfNode>() {
                    for branch in &if_node.branches {
                        collect_loopvar_paths(&branch.nodelist, loopvar, paths, path_to_slot);
                    }
                    continue;
                }
                // Nested ForNode: skip on shadowed loopvar; else descend.
                if let Some(inner_for) = node.as_any().downcast_ref::<ForNode>() {
                    if inner_for.loopvars.iter().any(|v| v == loopvar) {
                        continue;
                    }
                    collect_loopvar_paths(&inner_for.nodelist_loop, loopvar, paths, path_to_slot);
                    if let Some(empty) = &inner_for.nodelist_empty {
                        collect_loopvar_paths(empty, loopvar, paths, path_to_slot);
                    }
                    continue;
                }
                // Other tags: descend via Node::walk_children.
                node.walk_children(&mut |child_nodelist: &NodeList| {
                    collect_loopvar_paths(child_nodelist, loopvar, paths, path_to_slot);
                });
            }
            NodeEntry::Text(_) => {}
        }
    }
}

fn visit_filter_expression(
    fe: &FilterExpression,
    loopvar: &str,
    paths: &mut Vec<String>,
    path_to_slot: &mut std::collections::HashMap<String, u16>,
) {
    use crate::variable::FilterExpressionVar;

    if let FilterExpressionVar::Var(variable) = &fe.var {
        if let Some(parts) = variable.lookups() {
            // `loopvar.path` only; raw loopvar is already in context.
            if parts.len() >= 2 && parts[0] == loopvar {
                if let Some(rest) = variable.lookup_rest() {
                    let slot = match path_to_slot.get(rest) {
                        Some(&existing) => existing,
                        None => {
                            if paths.len() >= u16::MAX as usize {
                                return;
                            }
                            let new_slot = paths.len() as u16;
                            paths.push(rest.to_owned());
                            path_to_slot.insert(rest.to_owned(), new_slot);
                            new_slot
                        }
                    };
                    // Stamp the slot so render skips the HashMap probe.
                    variable.set_batch_slot(slot);
                }
            }
        }
    }

    // Filter args may also be variable refs; recurse so e.g.
    // `{{ x|default:app.fallback }}` picks up `fallback`.
    for parsed_filter in &fe.filters {
        for arg in &parsed_filter.args {
            if arg.is_lookup {
                if let Some(var) = &arg.variable {
                    if let Some(parts) = var.lookups() {
                        if parts.len() >= 2 && parts[0] == loopvar {
                            if let Some(rest) = var.lookup_rest() {
                                let slot = match path_to_slot.get(rest) {
                                    Some(&existing) => existing,
                                    None => {
                                        if paths.len() >= u16::MAX as usize {
                                            return;
                                        }
                                        let new_slot = paths.len() as u16;
                                        paths.push(rest.to_owned());
                                        path_to_slot.insert(rest.to_owned(), new_slot);
                                        new_slot
                                    }
                                };
                                var.set_batch_slot(slot);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl ForNode {
    /// Streams output to `out`. Shared by `render` and `render_annotated_into`.
    fn render_body(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        let _g = crate::prof::Guard::new("ForNode::render");
        let seq_value = resolve_if_value(py, &self.sequence, context);

        let items: Vec<Value> = match &seq_value {
            Value::List(items) => {
                if self.is_reversed {
                    items.iter().rev().cloned().collect()
                } else {
                    items.clone()
                }
            }
            v if v.as_str().is_some() => {
                let s = v.as_str().unwrap();
                let chars: Vec<Value> = s.chars().map(|c| Value::String(c.to_string())).collect();
                if self.is_reversed {
                    chars.into_iter().rev().collect()
                } else {
                    chars
                }
            }
            Value::PyObject(obj) => {
                let bound = obj.bind(py);
                if let Ok(iter) = bound.try_iter() {
                    let mut result = Vec::new();
                    // Exact-type fast paths skip Value::from's __html__
                    // FFI for each model instance in a queryset.
                    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};
                    for item in iter.flatten() {
                        let v = if item.is_exact_instance_of::<PyBool>() {
                            Value::Bool(item.extract::<bool>().unwrap_or(false))
                        } else if item.is_exact_instance_of::<PyInt>() {
                            match item.extract::<i64>() {
                                Ok(n) => Value::Int(n),
                                Err(_) => Value::PyObject(item.unbind()),
                            }
                        } else if item.is_exact_instance_of::<PyFloat>() {
                            match item.extract::<f64>() {
                                Ok(f) => Value::Float(f),
                                Err(_) => Value::PyObject(item.unbind()),
                            }
                        } else if item.is_exact_instance_of::<PyString>() {
                            match item.extract::<String>() {
                                Ok(s) => Value::String(s),
                                Err(_) => Value::PyObject(item.unbind()),
                            }
                        } else if item.is_none() {
                            Value::None
                        } else {
                            // Opaque: model instances, dicts/lists,
                            // SafeString (str subclass), custom classes.
                            Value::PyObject(item.unbind())
                        };
                        result.push(v);
                    }
                    if self.is_reversed {
                        result.reverse();
                    }
                    result
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };

        if items.is_empty() {
            if let Some(ref empty_nodelist) = self.nodelist_empty {
                return empty_nodelist.render_into(py, context, out);
            }
            return Ok(());
        }

        let len = items.len();
        out.reserve(items.len() * 32);

        context.push();

        // Skip the forloop dict when the body doesn't reference it.
        if self.body_uses_forloop {
            let parentloop = context.get("forloop").cloned();

            // Build once, mutate in place per iteration via index access.
            // Fixed order: 0 counter, 1 counter0, 2 revcounter,
            // 3 revcounter0, 4 first, 5 last, 6 length, [7 parentloop].
            let mut forloop = indexmap::IndexMap::with_capacity(8);
            forloop.insert("counter".to_owned(), Value::Int(0));
            forloop.insert("counter0".to_owned(), Value::Int(0));
            forloop.insert("revcounter".to_owned(), Value::Int(0));
            forloop.insert("revcounter0".to_owned(), Value::Int(0));
            forloop.insert("first".to_owned(), Value::Bool(false));
            forloop.insert("last".to_owned(), Value::Bool(false));
            forloop.insert("length".to_owned(), Value::Int(len as i64));
            if let Some(ref pl) = parentloop {
                forloop.insert("parentloop".to_owned(), pl.clone());
            }
            context.set("forloop".to_owned(), Value::Dict(forloop));
        }

        let single_loopvar = if self.loopvars.len() == 1 {
            Some(self.loopvars[0].clone())
        } else {
            None
        };

        // Pre-extract via `list(map(attrgetter, iterable))`: one FFI
        // hop covering N items × M attrs. Source iterable used
        // directly (no PyList::append per item).
        let pre_extracted: Option<Vec<Py<pyo3::PyAny>>> = match (
            single_loopvar.as_ref(),
            &self.batch_plan,
            &seq_value,
        ) {
            (Some(_), Some(plan), Value::PyObject(obj)) => {
                let bound = obj.bind(py);
                // Skip batching for the rare reversed-loop case
                // (alignment would require reversing iterable or result).
                if self.is_reversed {
                    None
                } else {
                    prebatch_extract_from_iterable(py, plan, &bound).ok()
                }
            }
            _ => None,
        };

        // Install the cache once; only `current_tuple` changes per iter.
        if let (Some(pre), Some(plan), Some(name)) = (
            pre_extracted.as_ref(),
            self.batch_plan.as_ref(),
            single_loopvar.as_ref(),
        ) {
            if !pre.is_empty() {
                context.loop_batch_cache = Some(crate::context::LoopBatchCache {
                    loopvar: name.clone(),
                    path_to_slot: std::sync::Arc::clone(plan.path_to_slot()),
                    current_tuple: pre[0].clone_ref(py),
                });
            }
        }

        // Pre-compute filtered columns: each `{{ x|filter:"arg" }}` in
        // the body applies once per row in a tight Rust loop. Empty
        // when there's no body program or no filtered columns.
        let mut pre_columns: Vec<Vec<crate::context::Value>> = Vec::new();
        if let (Some(pre), Some(name)) = (pre_extracted.as_ref(), single_loopvar.as_ref()) {
            let _ = name;
            let program_slot = self.body_program.get_or_init(|| {
                if let Some(name) = single_loopvar.as_ref() {
                    crate::body_program::compile_body_program(name, &self.nodelist_loop)
                } else {
                    None
                }
            });
            if let Some(program) = program_slot.as_ref() {
                if program.has_columns() {
                    pre_columns = program.precompute_columns(py, pre, context.autoescape);
                }
            }
        }

        for (i, item) in items.into_iter().enumerate() {
            if self.body_uses_forloop {
                if let Some(Value::Dict(forloop)) = context.get_in_topmost_mut("forloop") {
                    if let Some((_, v)) = forloop.get_index_mut(0) { *v = Value::Int((i + 1) as i64); }
                    if let Some((_, v)) = forloop.get_index_mut(1) { *v = Value::Int(i as i64); }
                    if let Some((_, v)) = forloop.get_index_mut(2) { *v = Value::Int((len - i) as i64); }
                    if let Some((_, v)) = forloop.get_index_mut(3) { *v = Value::Int((len - i - 1) as i64); }
                    if let Some((_, v)) = forloop.get_index_mut(4) { *v = Value::Bool(i == 0); }
                    if let Some((_, v)) = forloop.get_index_mut(5) { *v = Value::Bool(i == len - 1); }
                }
            }

            if let Some(ref name) = single_loopvar {
                // Update only `current_tuple`; loopvar/path_to_slot
                // stay loop-invariant.
                if let Some(pre) = pre_extracted.as_ref() {
                    if let (Some(tuple), Some(cache)) =
                        (pre.get(i), context.loop_batch_cache.as_mut())
                    {
                        cache.current_tuple = tuple.clone_ref(py);
                    }
                }
                context.set(name.clone(), item);
            } else {
                // Tuple unpacking: `Value::List` direct-index, PyObject
                // via __getitem__, anything else binds None per loopvar.
                let num_loopvars = self.loopvars.len();
                match &item {
                    Value::List(sub_items) => {
                        if sub_items.len() != num_loopvars {
                            context.pop();
                            return Err(TemplateError::PythonError(
                                pyo3::exceptions::PyValueError::new_err(format!(
                                    "Need {} values to unpack in for loop; got {}.",
                                    num_loopvars,
                                    sub_items.len(),
                                )),
                            ));
                        }
                        for (j, var_name) in self.loopvars.iter().enumerate() {
                            let val = sub_items.get(j).cloned().unwrap_or(Value::None);
                            context.set(var_name.clone(), val);
                        }
                    }
                    Value::String(s) => {
                        // String unpacking: iterate characters
                        let chars: Vec<Value> = s.chars().map(|c| Value::String(c.to_string())).collect();
                        if chars.len() != num_loopvars {
                            context.pop();
                            return Err(TemplateError::PythonError(
                                pyo3::exceptions::PyValueError::new_err(format!(
                                    "Need {} values to unpack in for loop; got {}.",
                                    num_loopvars,
                                    chars.len(),
                                )),
                            ));
                        }
                        for (j, var_name) in self.loopvars.iter().enumerate() {
                            context.set(var_name.clone(), chars[j].clone());
                        }
                    }
                    Value::SafeString(s) => {
                        let chars: Vec<Value> = s.chars().map(|c| Value::String(c.to_string())).collect();
                        if chars.len() != num_loopvars {
                            context.pop();
                            return Err(TemplateError::PythonError(
                                pyo3::exceptions::PyValueError::new_err(format!(
                                    "Need {} values to unpack in for loop; got {}.",
                                    num_loopvars,
                                    chars.len(),
                                )),
                            ));
                        }
                        for (j, var_name) in self.loopvars.iter().enumerate() {
                            context.set(var_name.clone(), chars[j].clone());
                        }
                    }
                    Value::PyObject(obj) => {
                        let bound = obj.bind(py);
                        // Collect all items first to validate count
                        let mut unpacked = Vec::new();
                        if let Ok(iter) = bound.try_iter() {
                            for item in iter.flatten() {
                                unpacked.push(Value::from(&item));
                            }
                        } else {
                            // Try __getitem__ for sequences
                            let mut j = 0;
                            loop {
                                match bound.get_item(j) {
                                    Ok(v) => unpacked.push(Value::from(&v)),
                                    Err(_) => break,
                                }
                                j += 1;
                            }
                        }
                        if unpacked.len() != num_loopvars {
                            context.pop();
                            return Err(TemplateError::PythonError(
                                pyo3::exceptions::PyValueError::new_err(format!(
                                    "Need {} values to unpack in for loop; got {}.",
                                    num_loopvars,
                                    unpacked.len(),
                                )),
                            ));
                        }
                        for (j, var_name) in self.loopvars.iter().enumerate() {
                            context.set(var_name.clone(), unpacked[j].clone());
                        }
                    }
                    Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::None => {
                        // Non-iterable: cannot unpack
                        context.pop();
                        return Err(TemplateError::PythonError(
                            pyo3::exceptions::PyValueError::new_err(format!(
                                "Need {} values to unpack in for loop; got 1.",
                                num_loopvars,
                            )),
                        ));
                    }
                    Value::Dict(_) => {
                        // Dict unpacking: iterate keys like Python
                        for var_name in self.loopvars.iter() {
                            context.set(var_name.clone(), Value::None);
                        }
                    }
                }
            }

            // Hot path: if the body has been compiled into a
            // `BodyProgram` and the batch cache is active, dispatch
            // through the opcode interpreter. Falls back to the
            // generic NodeList walk for non-batched loops, unspecialised
            // bodies, or empty iterables.
            let used_program = if context.loop_batch_cache.is_some() {
                let program_slot = self.body_program.get();
                if let Some(Some(program)) = program_slot {
                    // Build the &[&[Value]] view over pre_columns once,
                    // outside the inner loop body, to keep the call
                    // signature simple and avoid per-iter allocation.
                    let column_refs: Vec<&[crate::context::Value]> =
                        pre_columns.iter().map(|v| v.as_slice()).collect();
                    program.run(
                        py,
                        context,
                        out,
                        &self.nodelist_loop,
                        &column_refs,
                        i,
                    )?;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !used_program {
                self.nodelist_loop.render_into(py, context, out)?;
            }
        }

        // Tear down the batch cache so that code rendered after the loop
        // (or nested loops on the same context) doesn't see stale data.
        context.loop_batch_cache = None;

        context.pop();

        Ok(())
    }
}

impl Node for ForNode {
    fn render(&self, py: Python<'_>, context: &mut Context) -> Result<String, TemplateError> {
        let mut out = String::new();
        self.render_body(py, context, &mut out)?;
        Ok(out)
    }

    #[inline]
    fn render_annotated_into(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
    ) -> Result<(), TemplateError> {
        self.render_body(py, context, out)
    }

    impl_node_metadata!();

    fn child_nodelists(&self) -> &[&str] {
        &["nodelist_loop", "nodelist_empty"]
    }

    fn walk_children(&self, visit: &mut dyn FnMut(&NodeList)) {
        visit(&self.nodelist_loop);
        if let Some(ref nl) = self.nodelist_empty {
            visit(nl);
        }
    }
}

pub fn compile_for(parser: &mut Parser, token: &Token) -> Result<Box<dyn Node>, TemplateError> {
    let bits = token.split_contents();
    // Minimum: `for x in y` (4 tokens).
    if bits.len() < 4 {
        return Err(TemplateError::TemplateSyntaxError(
            "'for' statements should have at least four words: for x in y".into(),
        ));
    }

    let is_reversed = bits.last().map_or(false, |s| s == "reversed");

    let in_index = bits
        .iter()
        .position(|s| s == "in")
        .ok_or_else(|| {
            TemplateError::TemplateSyntaxError(
                "'for' statements should use the format 'for x in y': missing 'in'.".into(),
            )
        })?;

    // Loopvars: comma-separated between `for` and `in`.
    let loopvars_str = bits[1..in_index].join(" ");
    let loopvars: Vec<String> = loopvars_str
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    if loopvars.is_empty() {
        return Err(TemplateError::TemplateSyntaxError(
            "'for' tag received an invalid argument.".into(),
        ));
    }

    // Sequence: between `in` and optional `reversed`.
    let seq_end = if is_reversed { bits.len() - 1 } else { bits.len() };
    let sequence_token = bits[in_index + 1..seq_end].join(" ");
    let sequence = parser.compile_filter(&sequence_token)?;

    let nodelist_loop = parser.parse(&["empty", "endfor"])?;

    let next = parser.next_token();
    let tag = next.contents.split_whitespace().next().unwrap_or("");
    let nodelist_empty = if tag == "empty" {
        let nl = parser.parse(&["endfor"])?;
        parser.delete_first_token();
        Some(nl)
    } else {
        None
    };

    let body_uses_forloop = nodelist_references_forloop(&nodelist_loop)
        || nodelist_empty
            .as_ref()
            .map_or(false, nodelist_references_forloop);

    // Batch plan only for single-loopvar (attrgetter can't model
    // per-tuple-element extractors for tuple unpacking).
    let batch_plan = if loopvars.len() == 1 {
        ForBatchPlan::compute(&loopvars[0], &nodelist_loop).map(std::sync::Arc::new)
    } else {
        None
    };

    Ok(Box::new(ForNode {
        loopvars,
        sequence,
        is_reversed,
        nodelist_loop,
        nodelist_empty,
        body_uses_forloop,
        batch_plan,
        body_program: once_cell::sync::OnceCell::new(),
        token_field: None,
        origin_field: None,
    }))
}

/// Substring-scan node tokens for `forloop` or `ifchanged`. False
/// positives are safe (extra work); false negatives would break
/// `forloop.counter` or `{% ifchanged %}` state scoping. Used only
/// at parse time.
fn nodelist_references_forloop(nodelist: &NodeList) -> bool {
    fn token_needs_forloop(node: &dyn Node) -> bool {
        node.token()
            .map(|t| {
                t.contents.contains("forloop")
                    || t.contents.starts_with("ifchanged")
            })
            .unwrap_or(false)
    }

    fn walk(nodelist: &NodeList) -> bool {
        // NodeList::iter() skips Text entries (no template syntax).
        for node in nodelist.iter() {
            if token_needs_forloop(node) {
                return true;
            }
            let mut found_in_child = false;
            node.walk_children(&mut |child| {
                if !found_in_child && walk(child) {
                    found_in_child = true;
                }
            });
            if found_in_child {
                return true;
            }
        }
        false
    }

    walk(nodelist)
}
