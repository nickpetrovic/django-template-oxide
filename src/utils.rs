//! SafeString, html_escape, unescape_string_literal.

use compact_str::CompactString;

/// Mirrors `django.utils.safestring.SafeString` / `SafeData`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SafeString(pub CompactString);

impl SafeString {
    pub fn new(s: impl Into<CompactString>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SafeString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SafeString {
    fn from(s: &str) -> Self {
        Self(CompactString::new(s))
    }
}

impl From<String> for SafeString {
    fn from(s: String) -> Self {
        Self(CompactString::from(s))
    }
}

/// `django.utils.html.escape`.
#[must_use]
pub fn html_escape(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    html_escape_into(input, &mut output);
    output
}

/// Append-to-buffer variant; on the no-escape fast path this is a
/// single `push_str` (memcpy).
#[inline]
pub fn html_escape_into(input: &str, output: &mut String) {
    let bytes = input.as_bytes();
    let mut last = 0usize;

    output.reserve(input.len());

    for (i, &b) in bytes.iter().enumerate() {
        let replacement: &str = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&#x27;",
            _ => continue,
        };
        // Split points are ASCII bytes, so `last`/`i` are char boundaries.
        if last < i {
            output.push_str(&input[last..i]);
        }
        output.push_str(replacement);
        last = i + 1;
    }

    if last < bytes.len() {
        output.push_str(&input[last..]);
    }
}

/// `django.utils.html.conditional_escape`.
#[must_use]
pub fn conditional_escape(input: &str, is_safe: bool) -> SafeString {
    if is_safe {
        SafeString::new(input)
    } else {
        SafeString::new(html_escape(input))
    }
}

/// `django.utils.text.unescape_string_literal`. `None` for unquoted input.
pub fn unescape_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    if (quote != b'"' && quote != b'\'') || bytes[bytes.len() - 1] != quote {
        return None;
    }

    let inner = &s[1..s.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                result.push(next);
            }
        } else {
            result.push(c);
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("hello"), "hello");
        assert_eq!(html_escape("<b>bold</b>"), "&lt;b&gt;bold&lt;/b&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("\"quotes\""), "&quot;quotes&quot;");
        assert_eq!(html_escape("it's"), "it&#x27;s");
    }

    #[test]
    fn test_conditional_escape_safe() {
        let result = conditional_escape("<b>bold</b>", true);
        assert_eq!(result.as_str(), "<b>bold</b>");
    }

    #[test]
    fn test_conditional_escape_unsafe() {
        let result = conditional_escape("<b>bold</b>", false);
        assert_eq!(result.as_str(), "&lt;b&gt;bold&lt;/b&gt;");
    }

    #[test]
    fn test_unescape_string_literal_double_quotes() {
        assert_eq!(
            unescape_string_literal(r#""hello""#),
            Some("hello".to_string())
        );
    }

    #[test]
    fn test_unescape_string_literal_single_quotes() {
        assert_eq!(
            unescape_string_literal("'hello'"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn test_unescape_string_literal_escaped() {
        assert_eq!(
            unescape_string_literal(r#""he said \"hi\"""#),
            Some(r#"he said "hi""#.to_string())
        );
    }

    #[test]
    fn test_unescape_string_literal_not_quoted() {
        assert_eq!(unescape_string_literal("hello"), None);
        assert_eq!(unescape_string_literal(""), None);
        assert_eq!(unescape_string_literal("a"), None);
    }

    #[test]
    fn test_unescape_string_literal_mismatched_quotes() {
        assert_eq!(unescape_string_literal("\"hello'"), None);
    }
}
