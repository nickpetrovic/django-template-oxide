//! Rust-side accelerator for Django's own `NodeList.render`. Iterates
//! via `PyList_GET_ITEM`, short-circuits TextNode to `node.s`, and
//! writes into one pre-sized buffer. Caller wraps in `mark_safe`.

use pyo3::exceptions::PyRuntimeError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyString, PyType};

/// `nodelist` must be a list (Django's `NodeList`). Each node is
/// exact-type compared against `text_node_cls`. Exceptions from
/// `node.render_annotated` propagate.
#[pyfunction]
#[pyo3(signature = (nodelist, context, text_node_cls, _variable_node_cls=None))]
pub fn render_nodelist(
    py: Python<'_>,
    nodelist: &Bound<'_, PyAny>,
    context: &Bound<'_, PyAny>,
    text_node_cls: &Bound<'_, PyAny>,
    _variable_node_cls: Option<&Bound<'_, PyAny>>,
) -> PyResult<Py<PyString>> {
    // Django's NodeList subclasses list -> `PyList_GET_ITEM` fast path.
    let list = nodelist.cast::<PyList>().map_err(|_| {
        PyRuntimeError::new_err(
            "render_nodelist: nodelist must be a list (Django's NodeList \
             extends list - got a different type)",
        )
    })?;
    let len = list.len();

    // 32B/node is a conservative average; buffer doubles on overflow.
    let mut out = String::with_capacity(len.saturating_mul(32));

    let text_node_type = text_node_cls.cast::<PyType>().map_err(|_| {
        PyRuntimeError::new_err("render_nodelist: text_node_cls must be a type")
    })?;

    let s_attr = intern!(py, "s");
    let render_annotated = intern!(py, "render_annotated");

    for node in list.iter() {
        // Exact-type compare so subclasses (which may override
        // render_annotated) fall through to the slow path.
        if node.is_exact_instance(text_node_type) {
            // TextNode.render_annotated just returns self.s.
            let s = node.getattr(s_attr)?;
            let s_pystr = s
                .cast::<PyString>()
                .map_err(|_| PyRuntimeError::new_err("TextNode.s was not a str"))?;
            out.push_str(s_pystr.to_str()?);
        } else {
            // call_method1 with 1-tuple -> PyObject_CallMethodOneArg
            // (no intermediate PyTuple, vectorcall when supported).
            let result = node.call_method1(render_annotated, (context,))?;

            let result_pystr = result.cast::<PyString>().map_err(|_| {
                PyRuntimeError::new_err("Node.render_annotated returned a non-string value")
            })?;
            out.push_str(result_pystr.to_str()?);
        }
    }

    Ok(PyString::new(py, &out).unbind())
}
