//! Tokenize Django template strings into text/var/block/comment tokens
//! with line numbers (and positions in DebugLexer). Port of
//! `django.template.base.Lexer`, `DebugLexer`, and `smart_split`.

use once_cell::sync::Lazy;
use regex::Regex;

/// Django's `tag_re`: `{%...%}` / `{{...}}` / `{#...#}` (non-greedy).
static TAG_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\{%.*?%\}|\{\{.*?\}\}|\{#.*?#\})").expect("TAG_RE must compile"));

/// Django's `smart_split_re` (django.utils.text): quoted runs or
/// non-whitespace runs.
static SMART_SPLIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?x)
        ((?:
            [^\s'"]*
            (?:
                (?:"(?:[^"\\]|\\.)*" | '(?:[^'\\]|\\.)*')
                [^\s'"]*
            )+
        ) | \S+)"#,
    )
    .expect("SMART_SPLIT_RE must compile")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TokenType {
    Text = 0,
    Var = 1,
    Block = 2,
    Comment = 3,
}

impl std::fmt::Display for TokenType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text => write!(f, "Text"),
            Self::Var => write!(f, "Var"),
            Self::Block => write!(f, "Block"),
            Self::Comment => write!(f, "Comment"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub token_type: TokenType,
    pub contents: String,
    /// Set only by `DebugLexer`.
    pub position: Option<usize>,
    /// Length of the full source token (including `{% %}` delimiters).
    /// Set only by `DebugLexer`. Defaults to `contents.len()`.
    pub source_len: usize,
    pub lineno: usize,
}

impl Token {
    pub fn new(
        token_type: TokenType,
        contents: impl Into<String>,
        position: Option<usize>,
        lineno: usize,
    ) -> Self {
        let contents = contents.into();
        let source_len = contents.len();
        Self {
            token_type,
            position,
            source_len,
            contents,
            lineno,
        }
    }

    pub fn with_source_len(mut self, source_len: usize) -> Self {
        self.source_len = source_len;
        self
    }

    /// Mirrors `Token.split_contents`: smart_split, rejoining
    /// `_("...")` / `_('...')` markers split on whitespace.
    pub fn split_contents(&self) -> Vec<String> {
        let mut split: Vec<String> = Vec::new();
        let mut bits = smart_split(&self.contents).into_iter().peekable();

        while let Some(mut bit) = bits.next() {
            if bit.starts_with("_(\"") || bit.starts_with("_('") {
                // Sentinel closes the marker: `"` or `'` + `)`.
                let quote_char = bit.as_bytes()[2] as char;
                let sentinel = format!("{quote_char})");

                if !bit.ends_with(&sentinel) {
                    let mut trans_bit = vec![bit];
                    for next_bit in bits.by_ref() {
                        let done = next_bit.ends_with(&sentinel);
                        trans_bit.push(next_bit);
                        if done {
                            break;
                        }
                    }
                    bit = trans_bit.join(" ");
                }
            }
            split.push(bit);
        }

        split
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let truncated: String = self.contents.chars().take(20).collect();
        write!(f, "<{} token: \"{}...\">", self.token_type, truncated)
    }
}

/// Split on whitespace, respecting quotes. Port of
/// `django.utils.text.smart_split`.
pub fn smart_split(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    SMART_SPLIT_RE
        .find_iter(trimmed)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Mirrors `django.template.base.Lexer`.
#[derive(Debug, Clone)]
pub struct Lexer {
    template_string: String,
    verbatim: Option<String>,
}

impl Lexer {
    pub fn new(template_string: impl Into<String>) -> Self {
        Self {
            template_string: template_string.into(),
            verbatim: None,
        }
    }

    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut in_tag = false;
        let mut lineno: usize = 1;
        let mut result = Vec::new();

        // Replicates Python's `tag_re.split()` (alternating segments).
        let parts = split_keeping_delimiters(&TAG_RE, &self.template_string);

        for token_string in &parts {
            if !token_string.is_empty() {
                let token = self.create_token(token_string, None, lineno, in_tag);
                result.push(token);
                lineno += token_string.matches('\n').count();
            }
            in_tag = !in_tag;
        }

        result
    }

    fn create_token(
        &mut self,
        token_string: &str,
        position: Option<usize>,
        lineno: usize,
        in_tag: bool,
    ) -> Token {
        if in_tag && token_string.len() >= 4 {
            let token_start = &token_string[..2];

            if token_start == "{%" {
                let content = token_string[2..token_string.len() - 2].trim();

                if let Some(ref verbatim_end) = self.verbatim {
                    if content != verbatim_end.as_str() {
                        // Inside verbatim: emit as text.
                        return Token::new(TokenType::Text, token_string, position, lineno);
                    }
                    self.verbatim = None;
                } else if content == "verbatim" || content.starts_with("verbatim ") {
                    self.verbatim = Some(format!("end{content}"));
                }

                return Token::new(TokenType::Block, content, position, lineno);
            }

            if self.verbatim.is_none() {
                let content = token_string[2..token_string.len() - 2].trim();

                if token_start == "{{" {
                    return Token::new(TokenType::Var, content, position, lineno);
                }

                debug_assert!(token_start == "{#", "unexpected tag start: {token_start:?}");
                return Token::new(TokenType::Comment, content, position, lineno);
            }
        }

        Token::new(TokenType::Text, token_string, position, lineno)
    }
}

/// Mirrors `DebugLexer`: tracks character positions too.
#[derive(Debug, Clone)]
pub struct DebugLexer {
    template_string: String,
    verbatim: Option<String>,
}

impl DebugLexer {
    pub fn new(template_string: impl Into<String>) -> Self {
        Self {
            template_string: template_string.into(),
            verbatim: None,
        }
    }

    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut in_tag = false;
        let mut lineno: usize = 1;
        let mut position: usize = 0;
        let mut result = Vec::new();

        let parts = split_keeping_delimiters(&TAG_RE, &self.template_string);

        for token_string in &parts {
            if !token_string.is_empty() {
                let token = self.create_token(token_string, Some(position), lineno, in_tag);
                result.push(token);
                lineno += token_string.matches('\n').count();
                position += token_string.len();
            }
            in_tag = !in_tag;
        }

        result
    }

    fn create_token(
        &mut self,
        token_string: &str,
        position: Option<usize>,
        lineno: usize,
        in_tag: bool,
    ) -> Token {
        if in_tag && token_string.len() >= 4 {
            let token_start = &token_string[..2];

            if token_start == "{%" {
                let content = token_string[2..token_string.len() - 2].trim();

                if let Some(ref verbatim_end) = self.verbatim {
                    if content != verbatim_end.as_str() {
                        return Token::new(TokenType::Text, token_string, position, lineno);
                    }
                    self.verbatim = None;
                } else if content == "verbatim" || content.starts_with("verbatim ") {
                    self.verbatim = Some(format!("end{content}"));
                }

                return Token::new(TokenType::Block, content, position, lineno)
                    .with_source_len(token_string.len());
            }

            if self.verbatim.is_none() {
                let content = token_string[2..token_string.len() - 2].trim();

                if token_start == "{{" {
                    return Token::new(TokenType::Var, content, position, lineno)
                        .with_source_len(token_string.len());
                }

                debug_assert!(token_start == "{#", "unexpected tag start: {token_start:?}");
                return Token::new(TokenType::Comment, content, position, lineno)
                    .with_source_len(token_string.len());
            }
        }

        Token::new(TokenType::Text, token_string, position, lineno)
    }
}

/// Like Python's `re.split` with a capturing group: alternating non-match
/// and match segments in source order.
fn split_keeping_delimiters(re: &Regex, text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut last_end = 0;

    for m in re.find_iter(text) {
        let start = m.start();
        parts.push(text[last_end..start].to_string());
        parts.push(m.as_str().to_string());
        last_end = m.end();
    }

    parts.push(text[last_end..].to_string());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_var_token() {
        let mut lexer = Lexer::new("Hello {{ name }}");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);

        assert_eq!(tokens[0].token_type, TokenType::Text);
        assert_eq!(tokens[0].contents, "Hello ");

        assert_eq!(tokens[1].token_type, TokenType::Var);
        assert_eq!(tokens[1].contents, "name");
    }

    #[test]
    fn test_text_only() {
        let mut lexer = Lexer::new("Hello world");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, TokenType::Text);
        assert_eq!(tokens[0].contents, "Hello world");
    }

    #[test]
    fn test_block_tag() {
        let mut lexer = Lexer::new("{% if x %}yes{% endif %}");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);

        assert_eq!(tokens[0].token_type, TokenType::Block);
        assert_eq!(tokens[0].contents, "if x");

        assert_eq!(tokens[1].token_type, TokenType::Text);
        assert_eq!(tokens[1].contents, "yes");

        assert_eq!(tokens[2].token_type, TokenType::Block);
        assert_eq!(tokens[2].contents, "endif");
    }

    #[test]
    fn test_comment_token() {
        let mut lexer = Lexer::new("{# comment #}");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, TokenType::Comment);
        assert_eq!(tokens[0].contents, "comment");
    }

    #[test]
    fn test_comment_between_text() {
        let mut lexer = Lexer::new("before{# hidden #}after");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, TokenType::Text);
        assert_eq!(tokens[0].contents, "before");
        assert_eq!(tokens[1].token_type, TokenType::Comment);
        assert_eq!(tokens[1].contents, "hidden");
        assert_eq!(tokens[2].token_type, TokenType::Text);
        assert_eq!(tokens[2].contents, "after");
    }

    #[test]
    fn test_verbatim_basic() {
        let input = "{% verbatim %}{{ raw }}{% endverbatim %}";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, TokenType::Block);
        assert_eq!(tokens[0].contents, "verbatim");

        // Inside verbatim: the `{{ raw }}` should be emitted as TEXT, not VAR.
        assert_eq!(tokens[1].token_type, TokenType::Text);
        assert_eq!(tokens[1].contents, "{{ raw }}");

        assert_eq!(tokens[2].token_type, TokenType::Block);
        assert_eq!(tokens[2].contents, "endverbatim");
    }

    #[test]
    fn test_verbatim_named() {
        let input = "{% verbatim myblock %}{{ raw }}{% endverbatim myblock %}";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, TokenType::Block);
        assert_eq!(tokens[0].contents, "verbatim myblock");

        assert_eq!(tokens[1].token_type, TokenType::Text);
        assert_eq!(tokens[1].contents, "{{ raw }}");

        assert_eq!(tokens[2].token_type, TokenType::Block);
        assert_eq!(tokens[2].contents, "endverbatim myblock");
    }

    #[test]
    fn test_verbatim_block_tag_inside() {
        // Block tags inside verbatim should also be treated as text.
        let input = "{% verbatim %}{% if x %}{% endverbatim %}";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, TokenType::Block);
        assert_eq!(tokens[0].contents, "verbatim");

        assert_eq!(tokens[1].token_type, TokenType::Text);
        assert_eq!(tokens[1].contents, "{% if x %}");

        assert_eq!(tokens[2].token_type, TokenType::Block);
        assert_eq!(tokens[2].contents, "endverbatim");
    }

    #[test]
    fn test_verbatim_comment_inside() {
        // Comments inside verbatim should also be treated as text.
        let input = "{% verbatim %}{# comment #}{% endverbatim %}";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, TokenType::Block);
        assert_eq!(tokens[1].token_type, TokenType::Text);
        assert_eq!(tokens[1].contents, "{# comment #}");
        assert_eq!(tokens[2].token_type, TokenType::Block);
    }

    #[test]
    fn test_smart_split_simple() {
        assert_eq!(smart_split("hello world"), vec!["hello", "world"]);
    }

    #[test]
    fn test_smart_split_double_quoted() {
        assert_eq!(
            smart_split(r#"name "John Doe" age"#),
            vec!["name", r#""John Doe""#, "age"]
        );
    }

    #[test]
    fn test_smart_split_single_quoted() {
        assert_eq!(
            smart_split("name 'John Doe' age"),
            vec!["name", "'John Doe'", "age"]
        );
    }

    #[test]
    fn test_smart_split_escaped_quotes() {
        assert_eq!(
            smart_split(r#""escaped \" quote""#),
            vec![r#""escaped \" quote""#]
        );
    }

    #[test]
    fn test_smart_split_mixed_quotes() {
        assert_eq!(
            smart_split(r#"key="hello world" other='foo bar'"#),
            vec![r#"key="hello world""#, "other='foo bar'"]
        );
    }

    #[test]
    fn test_smart_split_empty() {
        let result: Vec<String> = smart_split("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_smart_split_whitespace_only() {
        let result: Vec<String> = smart_split("   ");
        assert!(result.is_empty());
    }

    #[test]
    fn test_lineno_tracking() {
        let input = "line1\n{{ var }}\nline3";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);

        assert_eq!(tokens[0].lineno, 1);
        assert_eq!(tokens[0].contents, "line1\n");

        assert_eq!(tokens[1].lineno, 2);
        assert_eq!(tokens[1].contents, "var");

        assert_eq!(tokens[2].lineno, 2);
        assert_eq!(tokens[2].contents, "\nline3");
    }

    #[test]
    fn test_lineno_multiline_text() {
        let input = "a\nb\nc\n{{ x }}";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens[0].lineno, 1);
        // 3 newlines in "a\nb\nc\n"; next token starts at line 4.
        assert_eq!(tokens[1].lineno, 4);
        assert_eq!(tokens[1].token_type, TokenType::Var);
    }

    #[test]
    fn test_debug_lexer_positions() {
        let input = "Hello {{ name }}";
        let mut lexer = DebugLexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);

        assert_eq!(tokens[0].position, Some(0));
        assert_eq!(tokens[0].contents, "Hello ");

        assert_eq!(tokens[1].position, Some(6));
        assert_eq!(tokens[1].contents, "name");
    }

    #[test]
    fn test_debug_lexer_block_positions() {
        let input = "{% if x %}yes{% endif %}";
        let mut lexer = DebugLexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens[0].position, Some(0));
        assert_eq!(tokens[1].position, Some(10));
        assert_eq!(tokens[2].position, Some(13));
    }

    #[test]
    fn test_split_contents_basic() {
        let token = Token::new(TokenType::Block, "if x == 1", None, 1);
        assert_eq!(token.split_contents(), vec!["if", "x", "==", "1"]);
    }

    #[test]
    fn test_split_contents_translation_marker() {
        let token = Token::new(
            TokenType::Block,
            r#"trans _("hello world") as greeting"#,
            None,
            1,
        );
        let parts = token.split_contents();
        assert_eq!(
            parts,
            vec!["trans", r#"_("hello world")"#, "as", "greeting"]
        );
    }

    #[test]
    fn test_split_contents_single_quote_translation() {
        let token = Token::new(
            TokenType::Block,
            "trans _('hello world') as greeting",
            None,
            1,
        );
        let parts = token.split_contents();
        assert_eq!(parts, vec!["trans", "_('hello world')", "as", "greeting"]);
    }

    #[test]
    fn test_split_keeping_delimiters() {
        let parts = split_keeping_delimiters(&TAG_RE, "a{{ b }}c");
        assert_eq!(parts, vec!["a", "{{ b }}", "c"]);
    }

    #[test]
    fn test_split_keeping_delimiters_no_match() {
        let parts = split_keeping_delimiters(&TAG_RE, "plain text");
        assert_eq!(parts, vec!["plain text"]);
    }

    #[test]
    fn test_split_keeping_delimiters_adjacent() {
        let parts = split_keeping_delimiters(&TAG_RE, "{{ a }}{{ b }}");
        assert_eq!(parts, vec!["", "{{ a }}", "", "{{ b }}", ""]);
    }

    #[test]
    fn test_all_token_types() {
        let input = "text{{ var }}{% block %}{# comment #}";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0].token_type, TokenType::Text);
        assert_eq!(tokens[1].token_type, TokenType::Var);
        assert_eq!(tokens[2].token_type, TokenType::Block);
        assert_eq!(tokens[3].token_type, TokenType::Comment);
    }

    #[test]
    fn test_empty_template() {
        let mut lexer = Lexer::new("");
        let tokens = lexer.tokenize();
        assert!(tokens.is_empty() || (tokens.len() == 1 && tokens[0].contents.is_empty()));
    }

    #[test]
    fn test_var_with_filter() {
        let mut lexer = Lexer::new("{{ name|lower }}");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, TokenType::Var);
        assert_eq!(tokens[0].contents, "name|lower");
    }
}
