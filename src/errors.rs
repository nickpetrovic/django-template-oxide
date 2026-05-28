//! Mirrors Django's template exceptions: `VariableDoesNotExist`,
//! `TemplateSyntaxError`, `TemplateDoesNotExist`.

use pyo3::prelude::*;

pyo3::create_exception!(
    django_template_oxide,
    TemplateSyntaxError,
    pyo3::exceptions::PyException,
    "Equivalent to django.template.exceptions.TemplateSyntaxError."
);

pyo3::create_exception!(
    django_template_oxide,
    VariableDoesNotExist,
    pyo3::exceptions::PyException,
    "Equivalent to django.template.base.VariableDoesNotExist."
);

pyo3::create_exception!(
    django_template_oxide,
    TemplateDoesNotExist,
    pyo3::exceptions::PyException,
    "Equivalent to django.template.exceptions.TemplateDoesNotExist."
);

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    /// `msg` is printf-style; `params` are substitutions
    /// (`VariableDoesNotExist(msg, params)` per Django).
    #[error("{}", format_msg(.msg, .params))]
    VariableDoesNotExist {
        msg: String,
        params: Vec<String>,
    },

    #[error("{0}")]
    TemplateSyntaxError(String),

    /// `tried`: `(loader, path)` pairs. `chain`: nested loader errors.
    #[error("{msg}")]
    TemplateDoesNotExist {
        msg: String,
        tried: Vec<(String, String)>,
        chain: Vec<String>,
    },

    /// Propagated as-is so user `except TypeError:` etc. still match.
    #[error("Python error: {0}")]
    PythonError(pyo3::PyErr),

    #[error("internal error: {0}")]
    Internal(String),
}

impl Clone for TemplateError {
    fn clone(&self) -> Self {
        match self {
            Self::VariableDoesNotExist { msg, params } => {
                Self::VariableDoesNotExist { msg: msg.clone(), params: params.clone() }
            }
            Self::TemplateSyntaxError(s) => Self::TemplateSyntaxError(s.clone()),
            Self::TemplateDoesNotExist { msg, tried, chain } => {
                Self::TemplateDoesNotExist { msg: msg.clone(), tried: tried.clone(), chain: chain.clone() }
            }
            Self::PythonError(e) => {
                Self::PythonError(Python::attach(|py| e.clone_ref(py)))
            }
            Self::Internal(s) => Self::Internal(s.clone()),
        }
    }
}

/// Python-`%`-style formatting for the `%s` / `%r` / `%d` patterns
/// Django uses. `%%` still collapses to `%` with no params.
fn format_msg(msg: &str, params: &[String]) -> String {
    let mut result = String::with_capacity(msg.len() + params.iter().map(|p| p.len()).sum::<usize>());
    let mut chars = msg.chars().peekable();
    let mut param_idx = 0;

    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.peek() {
                Some('s') | Some('r') | Some('d') => {
                    chars.next();
                    if param_idx < params.len() {
                        result.push_str(&params[param_idx]);
                        param_idx += 1;
                    } else {
                        result.push('%');
                        result.push('s');
                    }
                }
                Some('%') => {
                    chars.next();
                    result.push('%');
                }
                _ => {
                    result.push('%');
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

impl From<TemplateError> for PyErr {
    fn from(err: TemplateError) -> PyErr {
        match err {
            TemplateError::VariableDoesNotExist { .. } => {
                VariableDoesNotExist::new_err(err.to_string())
            }
            TemplateError::TemplateSyntaxError(msg) => {
                TemplateSyntaxError::new_err(msg)
            }
            TemplateError::TemplateDoesNotExist { msg, .. } => {
                TemplateDoesNotExist::new_err(msg)
            }
            TemplateError::PythonError(e) => e,
            TemplateError::Internal(msg) => {
                pyo3::exceptions::PyRuntimeError::new_err(msg)
            }
        }
    }
}

impl From<PyErr> for TemplateError {
    fn from(err: PyErr) -> Self {
        Python::attach(|py| {
            // Preserve Django's TemplateSyntaxError class for callers
            // (debug page, user `except TemplateSyntaxError`).
            let is_template_syntax_error = crate::python_cache::django(py)
                .ok()
                .map(|dj| {
                    err.matches(py, dj.template_syntax_error_cls.bind(py))
                        .unwrap_or(false)
                })
                .unwrap_or(false);

            if is_template_syntax_error {
                let msg = err.value(py).str()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| format!("{}", err));
                return TemplateError::TemplateSyntaxError(msg);
            }

            // Preserve original class across the round-trip.
            TemplateError::PythonError(err)
        })
    }
}

pub fn register_exceptions(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("TemplateSyntaxError", m.py().get_type::<TemplateSyntaxError>())?;
    m.add("VariableDoesNotExist", m.py().get_type::<VariableDoesNotExist>())?;
    m.add("TemplateDoesNotExist", m.py().get_type::<TemplateDoesNotExist>())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_msg_no_params() {
        assert_eq!(format_msg("simple message", &[]), "simple message");
    }

    #[test]
    fn test_format_msg_with_string_param() {
        assert_eq!(
            format_msg("Variable %s does not exist", &["foo".into()]),
            "Variable foo does not exist"
        );
    }

    #[test]
    fn test_format_msg_with_multiple_params() {
        assert_eq!(
            format_msg("%s failed in %s", &["lookup".into(), "context".into()]),
            "lookup failed in context"
        );
    }

    #[test]
    fn test_format_msg_escaped_percent() {
        assert_eq!(format_msg("100%% done", &[]), "100% done");
    }

    #[test]
    fn test_format_msg_repr_specifier() {
        assert_eq!(
            format_msg("value is %r", &["42".into()]),
            "value is 42"
        );
    }

    #[test]
    fn test_variable_does_not_exist_display() {
        let err = TemplateError::VariableDoesNotExist {
            msg: "Failed lookup for key [%s]".into(),
            params: vec!["name".into()],
        };
        assert_eq!(err.to_string(), "Failed lookup for key [name]");
    }

    #[test]
    fn test_template_syntax_error_display() {
        let err = TemplateError::TemplateSyntaxError("Invalid block tag".into());
        assert_eq!(err.to_string(), "Invalid block tag");
    }

    #[test]
    fn test_template_does_not_exist_display() {
        let err = TemplateError::TemplateDoesNotExist {
            msg: "base.html".into(),
            tried: vec![("loader1".into(), "path/base.html".into())],
            chain: vec![],
        };
        assert_eq!(err.to_string(), "base.html");
    }

    #[test]
    fn test_internal_error_display() {
        let err = TemplateError::Internal("boom".into());
        assert_eq!(err.to_string(), "internal error: boom");
    }

    #[test]
    fn test_error_is_clone() {
        let err = TemplateError::TemplateSyntaxError("test".into());
        let cloned = err.clone();
        assert_eq!(err.to_string(), cloned.to_string());
    }
}
