//! `{% for %}` / `{% empty %}` / `{% endfor %}`. Port of
//! `defaulttags.do_for` plus a `ForBatchPlan` extension: collect
//! `loopvar.path` references and resolve them via a single
//! `operator.attrgetter(*paths)(item)` per iteration.

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::impl_node_metadata;
use crate::lexer::Token;
use crate::nodes::{Node, NodeList, Origin};
use crate::parser::Parser;
use crate::variable::FilterExpression;

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
    string_if_invalid: &str,
) -> pyo3::PyResult<Vec<Py<pyo3::PyAny>>> {
    use pyo3::types::PyList;

    let getter = plan.get_attrgetter(py)?;
    let (map_fn, list_fn) = cached_map_list(py)?;

    // `list(map(getter, iterable))` in one FFI hop.
    let mapped = map_fn.bind(py).call1((getter.bind(py), iterable))?;
    let result_list = list_fn.bind(py).call1((mapped,))?;
    let result_list = result_list
        .cast::<PyList>()
        .map_err(|_| pyo3::exceptions::PyTypeError::new_err("expected list from list(map(...))"))?;

    let mut out = Vec::with_capacity(result_list.len());
    for tup in result_list.iter() {
        out.push(tup.unbind());
    }

    // attrgetter never auto-calls; Django does. Call callable leaves so
    // `{{ item.related.count }}` isn't rendered as a bound-method repr.
    autocall_callable_leaves(py, &mut out, string_if_invalid)?;

    Ok(out)
}

/// Auto-call callable leaves in the pre-extracted tuples (Django leaf
/// semantics). Callable slots are detected from row 0 (homogeneous iterable);
/// loops with none are left untouched, preserving the field-only fast path.
fn autocall_callable_leaves(
    py: pyo3::Python<'_>,
    rows: &mut [Py<pyo3::PyAny>],
    string_if_invalid: &str,
) -> pyo3::PyResult<()> {
    let Some(first) = rows.first() else {
        return Ok(());
    };
    let first = first.bind(py);
    let n_slots = first.len()?;

    let mut callable_slots: Vec<usize> = Vec::new();
    for i in 0..n_slots {
        let el = first.get_item(i)?;
        if crate::nodes::is_template_leaf_callable(&el) {
            callable_slots.push(i);
        }
    }
    if callable_slots.is_empty() {
        return Ok(());
    }

    for row in rows.iter_mut() {
        let bound = row.bind(py);
        let mut elems: Vec<pyo3::Bound<'_, pyo3::PyAny>> = Vec::with_capacity(n_slots);
        for i in 0..n_slots {
            elems.push(bound.get_item(i)?);
        }
        for &slot in &callable_slots {
            if crate::nodes::is_template_leaf_callable(&elems[slot]) {
                crate::nodes::maybe_call_template_callable(
                    py,
                    &mut elems[slot],
                    string_if_invalid,
                )?;
            }
        }
        *row = pyo3::types::PyTuple::new(py, &elems)?.unbind().into_any();
    }

    Ok(())
}

/// Cached `builtins.map` / `builtins.list` to skip FFI lookups per prebatch.
fn cached_map_list(
    py: pyo3::Python<'_>,
) -> pyo3::PyResult<(&'static Py<pyo3::PyAny>, &'static Py<pyo3::PyAny>)> {
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

    if let FilterExpressionVar::Var(variable) = &fe.var
        && let Some(parts) = variable.lookups()
    {
        // `loopvar.path` only; raw loopvar is already in context.
        if parts.len() >= 2
            && parts[0] == loopvar
            && let Some(rest) = variable.lookup_rest()
        {
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

    // Filter args may also be variable refs; recurse so e.g.
    // `{{ x|default:app.fallback }}` picks up `fallback`.
    for parsed_filter in &fe.filters {
        for arg in &parsed_filter.args {
            if arg.is_lookup
                && let Some(var) = &arg.variable
                && let Some(parts) = var.lookups()
                && parts.len() >= 2
                && parts[0] == loopvar
                && let Some(rest) = var.lookup_rest()
            {
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

fn unpack_item(py: Python<'_>, item: &Value, count: usize) -> Result<Vec<Value>, TemplateError> {
    let unpack_error = |got: usize| {
        TemplateError::PythonError(pyo3::exceptions::PyValueError::new_err(format!(
            "Need {count} values to unpack in for loop; got {got}. "
        )))
    };
    let parts: Vec<Value> = match item {
        Value::List(items) => items.clone(),
        Value::String(_) | Value::SafeString(_) => item
            .as_str()
            .expect("string variant")
            .chars()
            .map(|c| Value::String(c.to_string()))
            .collect(),
        other => {
            let obj = other.to_pyobject(py).into_bound(py);
            let length = match obj.len() {
                Ok(length) => length,
                Err(error) if error.is_instance_of::<pyo3::exceptions::PyTypeError>(py) => 1,
                Err(error) => return Err(error.into()),
            };
            if length != count {
                return Err(unpack_error(length));
            }
            let mut parts = Vec::with_capacity(count);
            for part in obj.try_iter()?.take(count) {
                parts.push(crate::nodes::value_from_pyany_fast(&part?));
            }
            return Ok(parts);
        }
    };
    if parts.len() != count {
        return Err(unpack_error(parts.len()));
    }
    Ok(parts)
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
        let seq_value = super::resolve_value_ignore_failures(py, &self.sequence, context)?;

        let mut python_sequence = None;
        let len_values = match &seq_value {
            Value::None => 0,
            Value::List(items) => items.len(),
            Value::String(s) => s.chars().count(),
            Value::SafeString(s) => s.chars().count(),
            other => {
                let mut sequence = other.to_pyobject(py).into_bound(py);
                if !sequence.is_exact_instance_of::<pyo3::types::PyList>()
                    && !sequence.is_exact_instance_of::<pyo3::types::PyTuple>()
                    && !sequence.hasattr(pyo3::intern!(py, "__len__"))?
                {
                    sequence = py.get_type::<pyo3::types::PyList>().call1((sequence,))?;
                }
                let len = sequence.len()?;
                python_sequence = Some(sequence);
                len
            }
        };

        if len_values == 0 {
            if let Some(ref empty_nodelist) = self.nodelist_empty {
                return empty_nodelist.render_into(py, context, out);
            }
            return Ok(());
        }

        let mut batch_source = None;
        let items: Vec<Value> = match (&seq_value, python_sequence) {
            (Value::List(items), _) => {
                if self.is_reversed {
                    items.iter().rev().cloned().collect()
                } else {
                    items.clone()
                }
            }
            (Value::String(_) | Value::SafeString(_), _) => {
                let s = seq_value.as_str().expect("matched String or SafeString");
                let chars: Vec<Value> = s.chars().map(|c| Value::String(c.to_string())).collect();
                if self.is_reversed {
                    chars.into_iter().rev().collect()
                } else {
                    chars
                }
            }
            (_, Some(sequence)) => {
                let iterated = match sequence.cast_exact::<pyo3::types::PyList>() {
                    Ok(list) if !self.is_reversed => list.clone(),
                    _ => {
                        let source = if self.is_reversed {
                            py.import("builtins")?
                                .getattr(pyo3::intern!(py, "reversed"))?
                                .call1((&sequence,))?
                        } else {
                            sequence
                        };
                        py.get_type::<pyo3::types::PyList>()
                            .call1((source,))?
                            .cast_into::<pyo3::types::PyList>()
                            .map_err(pyo3::PyErr::from)?
                    }
                };
                let items = iterated
                    .iter()
                    .map(|item| crate::nodes::value_from_pyany_fast(&item))
                    .collect();
                batch_source = Some(iterated);
                items
            }
            (_, None) => Vec::new(),
        };
        crate::filters::invalidate_locale_snapshot();

        out.reserve(items.len() * 32);

        context.push();
        let outer_batch_cache = context.loop_batch_cache.take();
        let outcome =
            self.render_iterations(py, context, out, items, len_values, batch_source.as_ref());
        context.loop_batch_cache = outer_batch_cache;
        context.pop();
        outcome
    }

    fn render_iterations(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
        items: Vec<Value>,
        len_values: usize,
        batch_source: Option<&Bound<'_, pyo3::types::PyList>>,
    ) -> Result<(), TemplateError> {
        let len = len_values as i64;

        if self.body_uses_forloop {
            let parentloop = context
                .get("forloop")
                .cloned()
                .unwrap_or_else(|| Value::Dict(Default::default()));

            let mut forloop =
                crate::context::ValueMap::with_capacity_and_hasher(8, Default::default());
            use compact_str::CompactString;
            forloop.insert(CompactString::const_new("parentloop"), parentloop);
            forloop.insert(CompactString::const_new("length"), Value::Int(len));
            forloop.insert(CompactString::const_new("counter0"), Value::Int(0));
            forloop.insert(CompactString::const_new("counter"), Value::Int(0));
            forloop.insert(CompactString::const_new("revcounter"), Value::Int(0));
            forloop.insert(CompactString::const_new("revcounter0"), Value::Int(0));
            forloop.insert(CompactString::const_new("first"), Value::Bool(false));
            forloop.insert(CompactString::const_new("last"), Value::Bool(false));
            context.set("forloop".to_owned(), Value::Dict(forloop));
        }

        let single_loopvar = if self.loopvars.len() == 1 {
            Some(self.loopvars[0].clone())
        } else {
            None
        };

        let pre_extracted: Option<Vec<Py<pyo3::PyAny>>> =
            match (single_loopvar.as_ref(), &self.batch_plan, batch_source) {
                (Some(_), Some(plan), Some(source)) => prebatch_extract_from_iterable(
                    py,
                    plan,
                    source.as_any(),
                    &context.string_if_invalid,
                )
                .ok(),
                _ => None,
            };
        crate::filters::invalidate_locale_snapshot();

        if let (Some(pre), Some(plan), Some(name)) = (
            pre_extracted.as_ref(),
            self.batch_plan.as_ref(),
            single_loopvar.as_ref(),
        ) && !pre.is_empty()
        {
            context.loop_batch_cache = Some(crate::context::LoopBatchCache {
                loopvar: name.clone(),
                path_to_slot: std::sync::Arc::clone(plan.path_to_slot()),
                current_tuple: pre[0].clone_ref(py),
            });
        }

        let mut pre_columns: Vec<Vec<Option<crate::context::Value>>> = Vec::new();
        if let (Some(pre), Some(name)) = (pre_extracted.as_ref(), single_loopvar.as_ref()) {
            let program_slot = self.body_program.get_or_init(|| {
                crate::body_program::compile_body_program(name, &self.nodelist_loop)
            });
            if let Some(program) = program_slot.as_ref()
                && program.has_columns()
            {
                pre_columns =
                    program.precompute_columns(py, pre, context.autoescape, context.use_tz);
            }
        }
        let column_refs: Vec<&[Option<crate::context::Value>]> =
            pre_columns.iter().map(Vec::as_slice).collect();

        for (index, item) in items.into_iter().enumerate() {
            let i = index as i64;
            if self.body_uses_forloop
                && let Some(Value::Dict(forloop)) = context.get_in_topmost_mut("forloop")
            {
                let counters = [
                    Value::Int(i),
                    Value::Int(i + 1),
                    Value::Int(len - i),
                    Value::Int(len - i - 1),
                    Value::Bool(i == 0),
                    Value::Bool(i == len - 1),
                ];
                for (offset, counter) in counters.into_iter().enumerate() {
                    if let Some((_, slot)) = forloop.get_index_mut(2 + offset) {
                        *slot = counter;
                    }
                }
            }

            if let Some(ref name) = single_loopvar {
                if let Some(pre) = pre_extracted.as_ref()
                    && let (Some(tuple), Some(cache)) =
                        (pre.get(index), context.loop_batch_cache.as_mut())
                {
                    cache.current_tuple = tuple.clone_ref(py);
                }
                context.set(name.clone(), item);
            } else {
                let unpacked = unpack_item(py, &item, self.loopvars.len())?;
                let layer: crate::context::ContextDict =
                    self.loopvars.iter().cloned().zip(unpacked).collect();
                context.push_with(layer);
                let rendered = self.nodelist_loop.render_into(py, context, out);
                context.pop();
                rendered?;
                continue;
            }

            let used_program = if context.loop_batch_cache.is_some() {
                let program_slot = self.body_program.get();
                if let Some(Some(program)) = program_slot {
                    program.run(py, context, out, &self.nodelist_loop, &column_refs, index)?;
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

    let is_reversed = bits.last().is_some_and(|s| s == "reversed");

    let in_index = bits.iter().position(|s| s == "in").ok_or_else(|| {
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
    let seq_end = if is_reversed {
        bits.len() - 1
    } else {
        bits.len()
    };
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
            .is_some_and(nodelist_references_forloop);

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

fn nodelist_references_forloop(nodelist: &NodeList) -> bool {
    fn token_needs_forloop(node: &dyn Node) -> bool {
        let any = node.as_any();
        any.is::<crate::django_drop_in::PyOpaqueNode>()
            || any.is::<crate::tags::loader_tags::IncludeNode>()
            || node
                .token()
                .map(|t| t.contents.contains("forloop") || t.contents.starts_with("ifchanged"))
                .unwrap_or(false)
    }

    fn walk(nodelist: &NodeList) -> bool {
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
