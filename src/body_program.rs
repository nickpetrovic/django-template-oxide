//! Bytecode-style for-loop body specialisation. When every body node
//! is specialisable, the compiler emits a flat opcode stream against
//! the `LoopBatchCache`'s pre-extracted tuple, eliminating per-iteration
//! dispatch and HashMap probes. Non-specialised nodes fall through via
//! `Op::InvokeNode`.

use std::sync::Arc;

use pyo3::prelude::*;

use crate::context::{Context, Value};
use crate::errors::TemplateError;
use crate::nodes::{Node, render_value_in_context_into, value_from_pyany_fast};
use crate::variable::NativeFilterRef;

/// Body-program instruction. Stays small for tight jump-table dispatch.
#[derive(Debug, Clone)]
pub enum Op {
    /// Index into `BodyProgram.text_pool`.
    EmitText(u32),
    /// Read tuple slot, stringify, autoescape if enabled.
    EmitSlot(u16),
    EmitFilterColumn(u32),
    /// Truthiness test on the tuple slot.
    JmpIfSlotFalsy {
        slot: u16,
        else_pc: u32,
    },
    Jmp(u32),
    /// Fall back to the original boxed Node at this NodeList index.
    InvokeNode(u32),
}

#[derive(Debug, Clone)]
pub struct ColumnSpec {
    pub slot: u16,
    pub native: NativeFilterRef,
    pub arg: Option<Value>,
}

impl ColumnSpec {
    fn args(&self) -> &[Value] {
        match &self.arg {
            Some(arg) => std::slice::from_ref(arg),
            None => &[],
        }
    }
}

/// Compiled for-loop body. Held by `ForNode`. `InvokeNode(idx)`
/// dispatches into the original NodeList so we don't have to clone
/// `Box<dyn Node>` (trait objects can't be cloned).
#[derive(Debug)]
pub struct BodyProgram {
    pub ops: Vec<Op>,
    pub text_pool: Vec<Arc<str>>,
    pub columns: Vec<ColumnSpec>,
}

impl BodyProgram {
    pub fn precompute_columns(
        &self,
        py: Python<'_>,
        tuples: &[Py<pyo3::PyAny>],
        autoescape: bool,
        use_tz: Option<bool>,
    ) -> Vec<Vec<Option<Value>>> {
        self.columns
            .iter()
            .map(|column| {
                tuples
                    .iter()
                    .map(|tuple| {
                        let item = tuple.bind(py).get_item(column.slot as usize).ok()?;
                        column.native.try_fast(
                            py,
                            &value_from_pyany_fast(&item),
                            column.args(),
                            autoescape,
                            use_tz,
                        )
                    })
                    .collect()
            })
            .collect()
    }

    pub fn has_columns(&self) -> bool {
        !self.columns.is_empty()
    }

    /// Execute. Reads from `context.loop_batch_cache.current_tuple`.
    /// The compiler guarantees PC and pool indices are in range.
    pub fn run(
        &self,
        py: Python<'_>,
        context: &mut Context,
        out: &mut String,
        body_nodelist: &crate::nodes::NodeList,
        pre_filter_results: &[&[Option<Value>]],
        row_index: usize,
    ) -> Result<(), TemplateError> {
        // Keep a tuple ref so we can also mut-borrow `context` for
        // `Op::InvokeNode`. `clone_ref` is an Arc bump.
        let tuple_py = {
            let cache = context
                .loop_batch_cache
                .as_ref()
                .expect("BodyProgram::run requires an active loop_batch_cache");
            cache.current_tuple.clone_ref(py)
        };
        let tuple = tuple_py.bind(py);

        let mut pc: usize = 0;
        let n = self.ops.len();
        while pc < n {
            match &self.ops[pc] {
                Op::EmitText(idx) => {
                    out.push_str(&self.text_pool[*idx as usize]);
                    pc += 1;
                }
                Op::EmitSlot(slot) => {
                    let value = value_from_pyany_fast(&tuple.get_item(*slot as usize)?);
                    render_value_in_context_into(&value, context, out)?;
                    pc += 1;
                }
                Op::EmitFilterColumn(column_idx) => {
                    let precomputed = pre_filter_results
                        .get(*column_idx as usize)
                        .and_then(|column| column.get(row_index))
                        .and_then(Option::as_ref);
                    match precomputed {
                        Some(value) => render_value_in_context_into(value, context, out)?,
                        None => {
                            let column = &self.columns[*column_idx as usize];
                            let input =
                                value_from_pyany_fast(&tuple.get_item(column.slot as usize)?);
                            let value = column.native.call(
                                py,
                                &input,
                                column.args(),
                                context.autoescape,
                                context.use_tz,
                            )?;
                            render_value_in_context_into(&value, context, out)?;
                        }
                    }
                    pc += 1;
                }
                Op::JmpIfSlotFalsy { slot, else_pc } => {
                    let truthy = tuple.get_item(*slot as usize)?.is_truthy()?;
                    if truthy {
                        pc += 1;
                    } else {
                        pc = *else_pc as usize;
                    }
                }
                Op::Jmp(target) => {
                    pc = *target as usize;
                }
                Op::InvokeNode(idx) => {
                    // Index into the original body NodeList; matches
                    // the position the compiler saw.
                    let entry = body_nodelist
                        .iter_entries()
                        .nth(*idx as usize)
                        .expect("InvokeNode index out of range");
                    match entry {
                        crate::nodes::NodeEntry::Text(s) => {
                            out.push_str(s);
                        }
                        crate::nodes::NodeEntry::Variable(v) => {
                            v.render_annotated_into(py, context, out)?;
                        }
                        crate::nodes::NodeEntry::Boxed(n) => {
                            n.render_annotated_into(py, context, out)
                                .map_err(|error| error.at_node_index(*idx as usize))?;
                        }
                    }
                    pc += 1;
                }
            }
        }
        Ok(())
    }
}

/// Compile a NodeList into a BodyProgram. `None` only on a structural
/// failure; nodes the compiler can't specialise emit `Op::InvokeNode`.
pub fn compile_body_program(
    loopvar: &str,
    nodelist: &crate::nodes::NodeList,
) -> Option<BodyProgram> {
    let mut builder = ProgramBuilder::new();
    if !compile_nodelist(&mut builder, nodelist, loopvar) {
        return None;
    }
    Some(builder.finish())
}

struct ProgramBuilder {
    ops: Vec<Op>,
    text_pool: Vec<Arc<str>>,
    columns: Vec<ColumnSpec>,
}

impl ProgramBuilder {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            text_pool: Vec::new(),
            columns: Vec::new(),
        }
    }

    fn intern_text(&mut self, s: &str) -> u32 {
        let idx = self.text_pool.len() as u32;
        self.text_pool.push(Arc::from(s));
        idx
    }

    fn register_column(&mut self, spec: ColumnSpec) -> u32 {
        for (i, existing) in self.columns.iter().enumerate() {
            if existing.slot == spec.slot
                && std::ptr::eq(existing.native, spec.native)
                && existing.arg == spec.arg
            {
                return i as u32;
            }
        }
        let idx = self.columns.len() as u32;
        self.columns.push(spec);
        idx
    }

    fn emit(&mut self, op: Op) -> u32 {
        let pc = self.ops.len() as u32;
        self.ops.push(op);
        pc
    }

    fn finish(self) -> BodyProgram {
        BodyProgram {
            ops: self.ops,
            text_pool: self.text_pool,
            columns: self.columns,
        }
    }
}

/// Walk a NodeList, emitting `Op::InvokeNode(idx)` for entries we
/// can't specialise. `false` only on structural failure.
fn compile_nodelist(
    builder: &mut ProgramBuilder,
    nodelist: &crate::nodes::NodeList,
    loopvar: &str,
) -> bool {
    use crate::nodes::NodeEntry;

    for (idx, entry) in nodelist.iter_entries().enumerate() {
        match entry {
            NodeEntry::Text(text) => {
                if !text.is_empty() {
                    let pool_idx = builder.intern_text(text);
                    builder.emit(Op::EmitText(pool_idx));
                }
            }
            NodeEntry::Variable(var_node) => {
                if !compile_variable(builder, var_node, loopvar) {
                    builder.emit(Op::InvokeNode(idx as u32));
                }
            }
            NodeEntry::Boxed(node) => {
                if !compile_boxed_node(builder, node.as_ref(), loopvar, idx as u32) {
                    builder.emit(Op::InvokeNode(idx as u32));
                }
            }
        }
    }
    true
}

fn compile_variable(
    builder: &mut ProgramBuilder,
    var_node: &crate::nodes::VariableNode,
    loopvar: &str,
) -> bool {
    let fe = &var_node.filter_expression;
    use crate::variable::FilterExpressionVar;

    let variable = match &fe.var {
        FilterExpressionVar::Var(v) => v,
        _ => return false,
    };

    let parts = match variable.lookups() {
        Some(p) => p,
        None => return false,
    };
    if parts.len() < 2 || parts[0] != loopvar || variable.translate {
        return false;
    }
    let slot = match variable.batch_slot() {
        Some(s) => s,
        None => return false,
    };

    if fe.filters.is_empty() {
        builder.emit(Op::EmitSlot(slot));
        return true;
    }

    if fe.filters.len() != 1 || fe.natives.len() != 1 {
        return false;
    }
    let pf = &fe.filters[0];
    let Some(native) = fe.natives[0] else {
        return false;
    };

    let arg: Option<Value> = match pf.args.len() {
        0 => None,
        1 => {
            let a = &pf.args[0];
            if a.is_lookup || a.variable.is_some() {
                return false;
            }
            Some(a.cached_constant().cloned().unwrap_or(Value::None))
        }
        _ => return false,
    };

    let column_idx = builder.register_column(ColumnSpec { slot, native, arg });
    builder.emit(Op::EmitFilterColumn(column_idx));
    true
}

/// Specialise a `Boxed` node. `false` falls through to `InvokeNode`.
fn compile_boxed_node(
    builder: &mut ProgramBuilder,
    node: &dyn crate::nodes::Node,
    loopvar: &str,
    _idx: u32,
) -> bool {
    use std::any::Any;

    let any_ref: &dyn Any = node.as_any();
    if let Some(if_node) = any_ref.downcast_ref::<crate::tags::IfNode>() {
        return compile_if(builder, if_node, loopvar);
    }
    false
}

/// `{% if loopvar.path %}...[else]...{% endif %}` where condition is a
/// single batched-slot reference (no and/or/not/comparison). Handles
/// the common ~80% case; complex conditions stay on the slow path.
fn compile_if(builder: &mut ProgramBuilder, if_node: &crate::tags::IfNode, loopvar: &str) -> bool {
    // Support `if cond` and optional `else`. No elif.
    let branches = if_node.branches();
    if branches.is_empty() || branches.len() > 2 {
        return false;
    }
    let then_branch = &branches[0];
    let else_branch = branches.get(1);

    let cond = match then_branch.condition() {
        Some(c) => c,
        None => return false,
    };
    if let Some(eb) = else_branch
        && eb.condition().is_some()
    {
        return false;
    }

    let slot = match cond.single_var_batch_slot(loopvar) {
        Some(s) => s,
        None => return false,
    };

    // JmpIfSlotFalsy -> else; then body; Jmp end; else body; end.
    let jmp_pc = builder.emit(Op::JmpIfSlotFalsy { slot, else_pc: 0 });

    if !compile_nodelist(builder, then_branch.nodelist(), loopvar) {
        return false;
    }

    if let Some(else_branch) = else_branch {
        let skip_pc = builder.emit(Op::Jmp(0));
        let else_start = builder.ops.len() as u32;
        if let Op::JmpIfSlotFalsy { else_pc, .. } = &mut builder.ops[jmp_pc as usize] {
            *else_pc = else_start;
        }
        if !compile_nodelist(builder, else_branch.nodelist(), loopvar) {
            return false;
        }
        let end_pc = builder.ops.len() as u32;
        if let Op::Jmp(t) = &mut builder.ops[skip_pc as usize] {
            *t = end_pc;
        }
    } else {
        let end_pc = builder.ops.len() as u32;
        if let Op::JmpIfSlotFalsy { else_pc, .. } = &mut builder.ops[jmp_pc as usize] {
            *else_pc = end_pc;
        }
    }
    true
}
