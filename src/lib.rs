pub mod body_program;
pub mod context;
pub mod django_drop_in;
pub mod django_integration;
pub mod errors;
pub mod filters;
pub mod lexer;
pub mod nodes;
pub mod parser;
pub mod prof;
pub mod py_bindings;
pub mod python_cache;
pub mod smartif;
pub mod tags;
pub mod template;
pub mod utils;
pub mod variable;

use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

#[pyfunction]
fn clear_template_cache_py() {
    tags::loader_tags::clear_template_cache();
}

/// Top-level PyO3 module exposed as `django_template_oxide._rust`.
#[pymodule]
fn _rust(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Cargo.toml version flows to `importlib.metadata` via
    // pyproject.toml's `dynamic = ["version"]`.
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    errors::register_exceptions(m)?;
    py_bindings::register(m)?;
    django_drop_in::register(m)?;
    m.add_function(wrap_pyfunction!(django_integration::render_nodelist, m)?)?;
    m.add_function(wrap_pyfunction!(prof::get_prof_stats, m)?)?;
    m.add_function(wrap_pyfunction!(prof::reset_prof_stats, m)?)?;
    m.add_function(wrap_pyfunction!(clear_template_cache_py, m)?)?;
    Ok(())
}
