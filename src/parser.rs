//! Template parser: port of `django.template.base.Parser`. Consumes
//! `Token`s and builds a `Node` tree.

use std::collections::HashMap;

use pyo3::prelude::*;

use crate::errors::TemplateError;
use crate::lexer::{Token, TokenType};
use crate::nodes::{Node, NodeList, Origin, TextNode, VariableNode};
use crate::variable::{FilterExpression, ParsedFilter};

pub type RustTagCompileFn =
    Box<dyn Fn(&mut Parser, &Token) -> Result<Box<dyn Node>, TemplateError>>;

/// Handler for `{% name %}`. Rust closure (built-ins) or Python
/// callable (loaded via `{% load %}`).
pub enum TagCompileFunc {
    Rust(RustTagCompileFn),
    /// Called with PyParser+PyToken; returns a Python Node we wrap
    /// in `PyOpaqueNode`.
    Python(Py<PyAny>),
}

impl std::fmt::Debug for TagCompileFunc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rust(_) => f.write_str("TagCompileFunc::Rust(<closure>)"),
            Self::Python(_) => f.write_str("TagCompileFunc::Python(<Py>)"),
        }
    }
}

pub type FilterFunc = Py<PyAny>;

/// State for a `{% cycle ... as NAME %}` so later `{% cycle NAME %}`
/// and `{% resetcycle NAME %}` reference the same cycle.
#[derive(Debug, Clone)]
pub struct NamedCycleState {
    pub render_key: String,
    pub cyclevars: Vec<crate::variable::FilterExpression>,
    pub silent: bool,
}

/// Mirrors `django.template.base.Parser`.
pub struct Parser {
    /// Reversed so `pop()` is O(1).
    tokens: Vec<Token>,

    pub tags: HashMap<String, TagCompileFunc>,

    /// Every Python tag compile fn ever offered via `add_python_library`,
    /// regardless of whether `self.tags` kept it. Cotton's
    /// `snapshot_parser_library` reads this so the captured library
    /// has Python-callable entries for tags Rust normally wins.
    pub python_tag_shadow: HashMap<String, Py<PyAny>>,

    pub filters: HashMap<String, FilterFunc>,

    /// `(command, token)` stack for unclosed-tag errors.
    command_stack: Vec<(String, Token)>,

    /// `{% block %}` names seen; mirrors `parser.__loaded_blocks`
    /// (loader_tags.py:69-75).
    pub loaded_block_names: std::collections::HashSet<String>,

    /// Mirrors `parser._namedCycleNodes` (defaulttags.py:43).
    pub named_cycles: HashMap<String, NamedCycleState>,

    /// For argument-less `{% resetcycle %}`. Mirrors `parser._last_cycle_node`.
    pub last_cycle_render_key: Option<String>,

    /// Lazily-created `parser.extra_data` scratchpad for third-party
    /// tags (template-partials, debug_toolbar). Persists across
    /// `PyParser.extra_data` accesses.
    pub python_extra_data: Option<Py<pyo3::types::PyDict>>,

    pub origin: Option<Origin>,

    pub libraries: HashMap<String, Py<PyAny>>,

    /// `{% partialdef %}` fragments; `Arc<Mutex>` so PartialDefNode
    /// (writes) and PartialNode (reads) share without lifetime issues.
    /// Mirrors `parser.extra_data.setdefault("partials", {})`.
    pub partials: std::sync::Arc<std::sync::Mutex<HashMap<String, std::sync::Arc<NodeList>>>>,
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("tokens_remaining", &self.tokens.len())
            .field("tags", &self.tags.keys().collect::<Vec<_>>())
            .field("filters", &self.filters.keys().collect::<Vec<_>>())
            .field("command_stack_depth", &self.command_stack.len())
            .finish()
    }
}

impl Parser {
    /// `tokens` in source order; we reverse internally for O(1) `pop`.
    pub fn new(tokens: Vec<Token>) -> Self {
        let mut reversed = tokens;
        reversed.reverse();

        Self {
            tokens: reversed,
            tags: HashMap::new(),
            python_tag_shadow: HashMap::new(),
            filters: HashMap::new(),
            command_stack: Vec::new(),
            loaded_block_names: std::collections::HashSet::new(),
            named_cycles: HashMap::new(),
            last_cycle_render_key: None,
            python_extra_data: None,
            origin: None,
            libraries: HashMap::new(),
            partials: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Mirrors `Parser.parse()`. Parses until a `parse_until` command
    /// or end of stream.
    pub fn parse(&mut self, parse_until: &[&str]) -> Result<NodeList, TemplateError> {
        let mut nodelist = NodeList::new();

        while !self.tokens.is_empty() {
            let token = self.next_token();

            match token.token_type {
                TokenType::Text => {
                    let node = TextNode::new(token.contents.as_str());
                    self.extend_nodelist(&mut nodelist, Box::new(node), &token)?;
                }

                TokenType::Var => {
                    if token.contents.is_empty() {
                        return Err(self.error(
                            &token,
                            TemplateError::TemplateSyntaxError(format!(
                                "Empty variable tag on line {}",
                                token.lineno,
                            )),
                        ));
                    }

                    let filter_expression = self
                        .compile_filter(&token.contents)
                        .map_err(|e| self.error(&token, e))?;

                    // Resolve each filter's Python callable; placeholder
                    // `None` for native-only filters (renderer detects).
                    let filter_funcs: Vec<Py<PyAny>> = pyo3::Python::attach(|py| {
                        filter_expression
                            .filters
                            .iter()
                            .map(|pf| match self.filters.get(&pf.name) {
                                Some(f) => f.clone_ref(py),
                                None => py.None(),
                            })
                            .collect()
                    });

                    let mut node =
                        Box::new(VariableNode::with_filters(filter_expression, filter_funcs));
                    // Inline extend_nodelist's metadata to keep the
                    // concrete `Box<VariableNode>` for `push_variable`.
                    node.set_token(token.clone());
                    if let Some(ref origin) = self.origin {
                        node.set_origin(origin.clone());
                    }
                    nodelist.push_variable(node);
                }

                TokenType::Block => {
                    let command = match token.contents.split_whitespace().next() {
                        Some(cmd) => cmd.to_owned(),
                        None => {
                            return Err(self.error(
                                &token,
                                TemplateError::TemplateSyntaxError(format!(
                                    "Empty block tag on line {}",
                                    token.lineno,
                                )),
                            ));
                        }
                    };

                    if parse_until.contains(&command.as_str()) {
                        self.prepend_token(token);
                        return Ok(nodelist);
                    }

                    self.command_stack.push((command.clone(), token.clone()));

                    let compile_func = match self.tags.get(&command) {
                        Some(f) => f,
                        None => {
                            return Err(self.invalid_block_tag(
                                &token,
                                &command,
                                if parse_until.is_empty() {
                                    None
                                } else {
                                    Some(parse_until)
                                },
                            ));
                        }
                    };

                    // Borrow-checker dance: the compile fn needs `&mut self`
                    // while it lives in `self.tags`. Rust variant uses a raw
                    // pointer (we don't mutate self.tags during the call);
                    // Python variant clones the Py<PyAny> Arc.
                    let compiled_result = match compile_func {
                        TagCompileFunc::Rust(rust_fn) => {
                            let rust_fn_ptr: *const RustTagCompileFn = rust_fn;
                            // SAFETY: pointer to a live HashMap value; we
                            // don't insert/remove from `self.tags` during
                            // the call so no reallocation invalidates it.
                            let rust_fn_ref: &RustTagCompileFn = unsafe { &*rust_fn_ptr };
                            rust_fn_ref(self, &token).map_err(|e| self.error(&token, e))?
                        }
                        TagCompileFunc::Python(py_fn) => {
                            let py_fn_clone = Python::attach(|py| py_fn.clone_ref(py));
                            dispatch_python_compile_fn(self, &py_fn_clone, &token)
                                .map_err(|e| self.error(&token, e))?
                        }
                    };

                    self.extend_nodelist(&mut nodelist, compiled_result, &token)?;
                    self.command_stack.pop();
                }

                TokenType::Comment => {
                    // Silently discarded (matches Django).
                }
            }
        }

        if !parse_until.is_empty() {
            return Err(self.unclosed_block_tag(parse_until));
        }

        Ok(nodelist)
    }

    /// Mirrors `Parser.extend_nodelist()`. Enforces `must_be_first`,
    /// sets token/origin, updates `contains_nontext`.
    fn extend_nodelist(
        &self,
        nodelist: &mut NodeList,
        mut node: Box<dyn Node>,
        token: &Token,
    ) -> Result<(), TemplateError> {
        if node.must_be_first() && nodelist.contains_nontext {
            return Err(self.error(
                token,
                TemplateError::TemplateSyntaxError(format!(
                    "{{% {} %}} must be the first tag in the template.",
                    token.contents,
                )),
            ));
        }

        node.set_token(token.clone());
        if let Some(ref origin) = self.origin {
            node.set_origin(origin.clone());
        }
        nodelist.push(node);
        Ok(())
    }

    /// Panics on empty stream; callers check `has_tokens()` first.
    pub fn next_token(&mut self) -> Token {
        self.tokens
            .pop()
            .expect("next_token() called on empty token stream")
    }

    pub fn prepend_token(&mut self, token: Token) {
        self.tokens.push(token);
    }

    pub fn delete_first_token(&mut self) {
        self.tokens.pop();
    }

    pub fn has_tokens(&self) -> bool {
        !self.tokens.is_empty()
    }

    pub fn tokens_remaining(&self) -> usize {
        self.tokens.len()
    }

    /// Mirrors `Parser.skip_past` (base.py:593-598).
    pub fn skip_past(&mut self, endtag: &str) -> Result<(), TemplateError> {
        while self.has_tokens() {
            let token = self.next_token();
            if token.token_type == TokenType::Block && token.contents == endtag {
                return Ok(());
            }
        }
        Err(self.unclosed_block_tag(&[endtag]))
    }

    /// Merge pre-extracted library maps.
    pub fn add_library_maps(
        &mut self,
        tags: HashMap<String, TagCompileFunc>,
        filters: HashMap<String, FilterFunc>,
    ) {
        self.tags.extend(tags);
        self.filters.extend(filters);
    }

    /// Mirrors `Parser.add_library` (base.py:668-670). Walks
    /// `lib.tags` and `lib.filters`.
    pub fn add_python_library(
        &mut self,
        py: pyo3::Python<'_>,
        lib: &pyo3::Bound<'_, pyo3::PyAny>,
    ) -> pyo3::PyResult<()> {
        use pyo3::types::PyDict;

        let tags_attr = lib.getattr(pyo3::intern!(py, "tags"))?;
        let tags_dict = tags_attr
            .cast::<PyDict>()
            .map_err(|_| pyo3::exceptions::PyTypeError::new_err("Library.tags must be a dict"))?;
        for (name, compile_fn) in tags_dict.iter() {
            let name_str: String = name.extract()?;
            let py_fn = compile_fn.unbind();

            // Always record in the shadow registry.
            self.python_tag_shadow
                .insert(name_str.clone(), py_fn.clone_ref(py));

            // Don't clobber native Rust tags: defaulttags' pure-Python
            // for/if/with iterate `self.nodelist_*` from Python, which
            // breaks against our non-iterable PyNodeList. New tags from
            // the library still merge; custom `{% load %}` tags are
            // distinct from Django's defaults so this is benign.
            if let std::collections::hash_map::Entry::Vacant(slot) = self.tags.entry(name_str) {
                slot.insert(TagCompileFunc::Python(py_fn));
            }
        }

        let filters_attr = lib.getattr(pyo3::intern!(py, "filters"))?;
        let filters_dict = filters_attr.cast::<PyDict>().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err("Library.filters must be a dict")
        })?;
        for (name, filter_fn) in filters_dict.iter() {
            let name_str: String = name.extract()?;
            self.filters.insert(name_str, filter_fn.unbind());
        }

        Ok(())
    }

    /// Mirrors `Parser.compile_filter`.
    pub fn compile_filter(&self, token: &str) -> Result<FilterExpression, TemplateError> {
        let mut fe = FilterExpression::parse(token, |filter_name| self.find_filter(filter_name))?;

        // Resolve Python filter callables once at parse time so every
        // consumer (with, if-arms, for-in, url, etc.) sees the right
        // function. Without this, Python-registered filters silently
        // turn into Value::None in tag args via `resolve_if_value`.
        if !fe.filters.is_empty() {
            let funcs: Vec<pyo3::Py<pyo3::PyAny>> = pyo3::Python::attach(|py| {
                fe.filters
                    .iter()
                    .map(|pf| match self.filters.get(&pf.name) {
                        Some(f) => f.clone_ref(py),
                        // Placeholder for native-Rust-only filters; the
                        // renderer only consults filter_funcs on a miss.
                        None => py.None(),
                    })
                    .collect()
            });
            fe.filter_funcs = std::sync::Arc::new(funcs);
        }

        Ok(fe)
    }

    /// Mirrors `Parser.find_filter`.
    pub fn find_filter(&self, filter_name: &str) -> Result<ParsedFilter, TemplateError> {
        if self.filters.contains_key(filter_name) {
            Ok(ParsedFilter {
                name: filter_name.to_owned(),
                args: Vec::new(),
            })
        } else {
            Err(TemplateError::TemplateSyntaxError(format!(
                "Invalid filter: '{}'",
                filter_name,
            )))
        }
    }

    /// Pass-through for now; `Parser.error` attaches the token in
    /// Django, but our `TemplateError` doesn't carry one yet.
    fn error(&self, _token: &Token, e: TemplateError) -> TemplateError {
        e
    }

    fn invalid_block_tag(
        &self,
        token: &Token,
        command: &str,
        parse_until: Option<&[&str]>,
    ) -> TemplateError {
        let msg = match parse_until {
            Some(expected) => {
                let expected_str: Vec<String> =
                    expected.iter().map(|s| format!("'{}'", s)).collect();
                format!(
                    "Invalid block tag on line {}: '{}', expected {}.",
                    token.lineno,
                    command,
                    expected_str.join(", "),
                )
            }
            None => {
                format!("Invalid block tag on line {}: '{}'.", token.lineno, command,)
            }
        };
        self.error(token, TemplateError::TemplateSyntaxError(msg))
    }

    fn unclosed_block_tag(&mut self, parse_until: &[&str]) -> TemplateError {
        let (command, token) = self
            .command_stack
            .pop()
            .expect("unclosed_block_tag called with empty command_stack");
        let expected_str: Vec<String> = parse_until.iter().map(|s| format!("'{}'", s)).collect();
        let msg = format!(
            "Unclosed tag on line {}: '{}'. Looking for one of: {}.",
            token.lineno,
            command,
            expected_str.join(", "),
        );
        self.error(&token, TemplateError::TemplateSyntaxError(msg))
    }
}

/// Call a Python tag compile fn and wrap the returned Node in
/// `PyOpaqueNode`. SAFETY: the `*mut Parser` cast is sound because we
/// hold an exclusive borrow and only pass it to PyParser for this call.
fn dispatch_python_compile_fn(
    parser: &mut Parser,
    py_compile_fn: &Py<pyo3::PyAny>,
    token: &Token,
) -> Result<Box<dyn Node>, TemplateError> {
    use pyo3::prelude::*;

    Python::attach(|py| -> PyResult<Box<dyn Node>> {
        // SAFETY: see fn-level docs.
        let py_parser = Py::new(py, unsafe {
            crate::django_drop_in::PyParser::from_raw(parser as *mut Parser)
        })?;
        let py_token = Py::new(
            py,
            crate::django_drop_in::PyToken::from_rust_token(py, token)?,
        )?;

        let node_obj = py_compile_fn.bind(py).call1((&py_parser, &py_token))?;

        // Mirrors Django's `Parser.parse` which does
        //   node.token = token
        //   node.origin = self.origin
        // after the compile function returns.
        let _ = node_obj.setattr(pyo3::intern!(py, "token"), &py_token);
        if let Some(ref origin) = parser.origin {
            let origin_mod = py.import("django.template.base")?;
            let origin_cls = origin_mod.getattr(pyo3::intern!(py, "Origin"))?;
            let kwargs = pyo3::types::PyDict::new(py);
            kwargs.set_item(pyo3::intern!(py, "name"), &origin.name)?;
            if let Some(ref tn) = origin.template_name {
                kwargs.set_item(pyo3::intern!(py, "template_name"), tn)?;
            }
            let py_origin = origin_cls.call((), Some(&kwargs))?;
            let _ = node_obj.setattr(pyo3::intern!(py, "origin"), py_origin);
        }

        Ok(Box::new(crate::django_drop_in::PyOpaqueNode::new(node_obj.unbind())) as Box<dyn Node>)
    })
    .map_err(TemplateError::from)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::{Lexer, Token, TokenType};

    fn lex(template: &str) -> Vec<Token> {
        Lexer::new(template).tokenize()
    }

    #[test]
    fn test_parse_text_only() {
        let tokens = lex("Hello world");
        let mut parser = Parser::new(tokens);
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
        assert!(!nodelist.contains_nontext);
    }

    #[test]
    fn test_parse_empty_template() {
        let mut parser = Parser::new(vec![]);
        let nodelist = parser.parse(&[]).unwrap();
        assert!(nodelist.is_empty());
    }

    #[test]
    fn test_parse_simple_variable() {
        let tokens = lex("{{ name }}");
        let mut parser = Parser::new(tokens);
        let nodelist = parser.parse(&[]).unwrap();

        assert_eq!(nodelist.len(), 1);
        assert!(nodelist.contains_nontext);
    }

    #[test]
    fn test_parse_text_and_variable() {
        let tokens = lex("Hello {{ name }}!");
        let mut parser = Parser::new(tokens);
        let nodelist = parser.parse(&[]).unwrap();

        // "Hello ", {{ name }}, "!"
        assert_eq!(nodelist.len(), 3);
        assert!(nodelist.contains_nontext);
    }

    #[test]
    fn test_parse_empty_variable_tag_errors() {
        // Construct an empty var token directly (the lexer wouldn't normally
        // produce this, but Django's parser handles the case).
        let tokens = vec![Token::new(TokenType::Var, "", None, 1)];
        let mut parser = Parser::new(tokens);
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Empty variable tag"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_block_tag_errors() {
        let tokens = lex("{% unknown %}");
        let mut parser = Parser::new(tokens);
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Invalid block tag"), "got: {}", msg);
                assert!(msg.contains("unknown"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_empty_block_tag_errors() {
        let tokens = vec![Token::new(TokenType::Block, "", None, 1)];
        let mut parser = Parser::new(tokens);
        let err = parser.parse(&[]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Empty block tag"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_until_stops_at_expected_tag() {
        let tokens = lex("Hello{% endif %}");
        let mut parser = Parser::new(tokens);

        // Parse until we hit "endif".
        let nodelist = parser.parse(&["endif"]).unwrap();

        // Should have consumed "Hello" but stopped at "endif".
        assert_eq!(nodelist.len(), 1); // just "Hello"
        assert!(parser.has_tokens()); // "endif" was prepended back
    }

    #[test]
    fn test_parse_until_unclosed_tag_errors() {
        // parse_until expects "endif" but we never provide it.
        // We need a command on the stack for the error message.
        let tokens = lex("Hello");
        let mut parser = Parser::new(tokens);
        parser.command_stack.push((
            "if".to_owned(),
            Token::new(TokenType::Block, "if x", None, 1),
        ));

        let err = parser.parse(&["endif"]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Unclosed tag"), "got: {}", msg);
                assert!(msg.contains("if"), "got: {}", msg);
                assert!(msg.contains("endif"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_next_token_and_prepend() {
        let tokens = vec![
            Token::new(TokenType::Text, "a", None, 1),
            Token::new(TokenType::Text, "b", None, 1),
        ];
        let mut parser = Parser::new(tokens);

        let first = parser.next_token();
        assert_eq!(first.contents, "a");

        parser.prepend_token(first.clone());
        let again = parser.next_token();
        assert_eq!(again.contents, "a");
    }

    #[test]
    fn test_delete_first_token() {
        let tokens = vec![
            Token::new(TokenType::Text, "a", None, 1),
            Token::new(TokenType::Text, "b", None, 1),
        ];
        let mut parser = Parser::new(tokens);

        parser.delete_first_token(); // removes "a"
        let next = parser.next_token();
        assert_eq!(next.contents, "b");
    }

    #[test]
    fn test_comments_are_discarded() {
        let tokens = lex("before{# comment #}after");
        let mut parser = Parser::new(tokens);
        let nodelist = parser.parse(&[]).unwrap();

        // The comment token should be silently skipped.
        assert_eq!(nodelist.len(), 2); // "before" and "after"
    }

    #[test]
    fn test_registered_tag_is_called() {
        let tokens = lex("{% hello %}");
        let mut parser = Parser::new(tokens);

        // Register a simple tag that produces a TextNode.
        parser.tags.insert(
            "hello".to_owned(),
            TagCompileFunc::Rust(Box::new(|_parser: &mut Parser, _token: &Token| {
                Ok(Box::new(TextNode::new("world")) as Box<dyn Node>)
            })),
        );

        let nodelist = parser.parse(&[]).unwrap();
        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_tag_calling_parse_until() {
        // Register an "if" tag that parses until "endif", then consumes
        // the "endif" token and returns a TextNode wrapping the child count.
        let tokens = lex("{% if %}inner text{% endif %}");
        let mut parser = Parser::new(tokens);

        parser.tags.insert(
            "if".to_owned(),
            TagCompileFunc::Rust(Box::new(|parser: &mut Parser, _token: &Token| {
                let children = parser.parse(&["endif"])?;
                parser.delete_first_token(); // consume {% endif %}
                let msg = format!("children:{}", children.len());
                Ok(Box::new(TextNode::new(msg)) as Box<dyn Node>)
            })),
        );

        let nodelist = parser.parse(&[]).unwrap();
        assert_eq!(nodelist.len(), 1);
    }

    #[test]
    fn test_find_filter_missing() {
        let parser = Parser::new(vec![]);
        let err = parser.find_filter("nonexistent").unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Invalid filter"), "got: {}", msg);
                assert!(msg.contains("nonexistent"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_compile_filter_simple_variable() {
        let parser = Parser::new(vec![]);
        let fe = parser.compile_filter("name").unwrap();
        assert_eq!(fe.token, "name");
        assert!(fe.is_var);
        assert!(fe.filters.is_empty());
    }

    #[test]
    fn test_invalid_block_tag_with_expected() {
        let tokens = lex("{% bogus %}");
        let mut parser = Parser::new(tokens);

        // Push a command stack entry so unclosed_block_tag doesn't panic.
        parser.command_stack.push((
            "if".to_owned(),
            Token::new(TokenType::Block, "if x", None, 1),
        ));

        let err = parser.parse(&["endif"]).unwrap_err();

        match err {
            TemplateError::TemplateSyntaxError(msg) => {
                assert!(msg.contains("Invalid block tag"), "got: {}", msg);
                assert!(msg.contains("bogus"), "got: {}", msg);
                assert!(msg.contains("endif"), "got: {}", msg);
            }
            other => panic!("expected TemplateSyntaxError, got {:?}", other),
        }
    }

    #[test]
    fn test_origin_set_on_nodes() {
        let tokens = lex("Hello {{ name }}");
        let mut parser = Parser::new(tokens);
        parser.origin = Some(Origin::new("test.html"));

        let nodelist = parser.parse(&[]).unwrap();

        // Both nodes should have the origin set.
        for node in nodelist.iter() {
            assert!(node.origin().is_some());
            assert_eq!(node.origin().unwrap().name, "test.html");
        }
    }
}
