//! Pratt parser for `{% if %}`. Port of `django/template/smartif.py`.
//! `nud` = null denotation, `led` = left denotation, `lbp` = left binding power.

use crate::errors::TemplateError;

#[derive(Debug, Clone)]
pub enum IfExpr {
    Literal(IfValue),
    Prefix {
        op: PrefixOp,
        operand: Box<IfExpr>,
    },
    Infix {
        op: InfixOp,
        left: Box<IfExpr>,
        right: Box<IfExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixOp {
    Not,
}

/// Binding powers match Python/Django precedence: or=6, and=7,
/// in/notin=9, comparisons=10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfixOp {
    Or,
    And,
    In,
    NotIn,
    Is,
    IsNot,
    Eq,
    NotEq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl InfixOp {
    fn binding_power(self) -> u8 {
        match self {
            Self::Or => 6,
            Self::And => 7,
            Self::In | Self::NotIn => 9,
            Self::Is | Self::IsNot | Self::Eq | Self::NotEq |
            Self::Gt | Self::Gte | Self::Lt | Self::Lte => 10,
        }
    }
}

impl PrefixOp {
    fn binding_power(self) -> u8 {
        match self {
            Self::Not => 8,
        }
    }
}

/// Opaque literal; resolved at render time by the caller's `create_var`.
#[derive(Debug, Clone)]
pub enum IfValue {
    Token(String),
}

#[derive(Debug, Clone)]
enum IfToken {
    Literal(IfValue),
    Prefix(PrefixOp),
    Infix(InfixOp),
    End,
}

impl IfToken {
    fn lbp(&self) -> u8 {
        match self {
            Self::Literal(_) | Self::End => 0,
            Self::Prefix(op) => op.binding_power(),
            Self::Infix(op) => op.binding_power(),
        }
    }

    fn display(&self) -> &str {
        match self {
            Self::Literal(IfValue::Token(s)) => s.as_str(),
            Self::Prefix(PrefixOp::Not) => "not",
            Self::Infix(InfixOp::Or) => "or",
            Self::Infix(InfixOp::And) => "and",
            Self::Infix(InfixOp::In) => "in",
            Self::Infix(InfixOp::NotIn) => "not in",
            Self::Infix(InfixOp::Is) => "is",
            Self::Infix(InfixOp::IsNot) => "is not",
            Self::Infix(InfixOp::Eq) => "==",
            Self::Infix(InfixOp::NotEq) => "!=",
            Self::Infix(InfixOp::Gt) => ">",
            Self::Infix(InfixOp::Gte) => ">=",
            Self::Infix(InfixOp::Lt) => "<",
            Self::Infix(InfixOp::Lte) => "<=",
            Self::End => "(end)",
        }
    }
}

fn translate_token(token: &str) -> IfToken {
    match token {
        "or" => IfToken::Infix(InfixOp::Or),
        "and" => IfToken::Infix(InfixOp::And),
        "not" => IfToken::Prefix(PrefixOp::Not),
        "in" => IfToken::Infix(InfixOp::In),
        "not in" => IfToken::Infix(InfixOp::NotIn),
        "is" => IfToken::Infix(InfixOp::Is),
        "is not" => IfToken::Infix(InfixOp::IsNot),
        "==" => IfToken::Infix(InfixOp::Eq),
        "!=" => IfToken::Infix(InfixOp::NotEq),
        ">" => IfToken::Infix(InfixOp::Gt),
        ">=" => IfToken::Infix(InfixOp::Gte),
        "<" => IfToken::Infix(InfixOp::Lt),
        "<=" => IfToken::Infix(InfixOp::Lte),
        other => IfToken::Literal(IfValue::Token(other.to_string())),
    }
}

/// Tokenize, combining `"is", "not"` -> `"is not"` and `"not", "in"`
/// -> `"not in"`, matching `IfParser.__init__`.
fn tokenize(raw_tokens: &[&str]) -> Vec<IfToken> {
    let mut result = Vec::with_capacity(raw_tokens.len());
    let mut i = 0;
    let n = raw_tokens.len();

    while i < n {
        let token = raw_tokens[i];
        if token == "is" && i + 1 < n && raw_tokens[i + 1] == "not" {
            result.push(translate_token("is not"));
            i += 2;
        } else if token == "not" && i + 1 < n && raw_tokens[i + 1] == "in" {
            result.push(translate_token("not in"));
            i += 2;
        } else {
            result.push(translate_token(token));
            i += 1;
        }
    }

    result
}

pub struct IfParser {
    tokens: Vec<IfToken>,
    pos: usize,
}

impl IfParser {
    pub fn new(raw_tokens: &[&str]) -> Self {
        Self {
            tokens: tokenize(raw_tokens),
            pos: 0,
        }
    }

    pub fn parse(&mut self) -> Result<IfExpr, TemplateError> {
        let result = self.expression(0)?;
        if self.pos < self.tokens.len() {
            let tok = &self.tokens[self.pos];
            return Err(TemplateError::TemplateSyntaxError(format!(
                "Unused '{}' at end of if expression.",
                tok.display()
            )));
        }
        Ok(result)
    }

    fn current_token(&self) -> &IfToken {
        self.tokens.get(self.pos).unwrap_or(&IfToken::End)
    }

    fn advance(&mut self) -> IfToken {
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            self.pos += 1;
            tok
        } else {
            IfToken::End
        }
    }

    fn expression(&mut self, rbp: u8) -> Result<IfExpr, TemplateError> {
        let t = self.advance();
        let mut left = self.nud(t)?;

        while rbp < self.current_token().lbp() {
            let t = self.advance();
            left = self.led(left, t)?;
        }

        Ok(left)
    }

    fn nud(&mut self, token: IfToken) -> Result<IfExpr, TemplateError> {
        match token {
            IfToken::Literal(val) => Ok(IfExpr::Literal(val)),
            IfToken::Prefix(op) => {
                let operand = self.expression(op.binding_power())?;
                Ok(IfExpr::Prefix {
                    op,
                    operand: Box::new(operand),
                })
            }
            IfToken::Infix(op) => Err(TemplateError::TemplateSyntaxError(format!(
                "Not expecting '{}' in this position in if tag.",
                match op {
                    InfixOp::Or => "or",
                    InfixOp::And => "and",
                    InfixOp::In => "in",
                    InfixOp::NotIn => "not in",
                    InfixOp::Is => "is",
                    InfixOp::IsNot => "is not",
                    InfixOp::Eq => "==",
                    InfixOp::NotEq => "!=",
                    InfixOp::Gt => ">",
                    InfixOp::Gte => ">=",
                    InfixOp::Lt => "<",
                    InfixOp::Lte => "<=",
                }
            ))),
            IfToken::End => Err(TemplateError::TemplateSyntaxError(
                "Unexpected end of expression in if tag.".to_string(),
            )),
        }
    }

    fn led(&mut self, left: IfExpr, token: IfToken) -> Result<IfExpr, TemplateError> {
        match token {
            IfToken::Infix(op) => {
                let right = self.expression(op.binding_power())?;
                Ok(IfExpr::Infix {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                })
            }
            _ => Err(TemplateError::TemplateSyntaxError(format!(
                "Not expecting '{}' as infix operator in if tag.",
                token.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_tokens(tokens: &[&str]) -> Result<IfExpr, TemplateError> {
        IfParser::new(tokens).parse()
    }

    #[test]
    fn test_simple_literal() {
        let expr = parse_tokens(&["x"]).unwrap();
        match expr {
            IfExpr::Literal(IfValue::Token(s)) => assert_eq!(s, "x"),
            _ => panic!("Expected literal"),
        }
    }

    #[test]
    fn test_not_prefix() {
        let expr = parse_tokens(&["not", "x"]).unwrap();
        match expr {
            IfExpr::Prefix { op: PrefixOp::Not, .. } => {}
            _ => panic!("Expected not prefix"),
        }
    }

    #[test]
    fn test_and_infix() {
        let expr = parse_tokens(&["x", "and", "y"]).unwrap();
        match expr {
            IfExpr::Infix { op: InfixOp::And, .. } => {}
            _ => panic!("Expected and infix"),
        }
    }

    #[test]
    fn test_or_infix() {
        let expr = parse_tokens(&["x", "or", "y"]).unwrap();
        match expr {
            IfExpr::Infix { op: InfixOp::Or, .. } => {}
            _ => panic!("Expected or infix"),
        }
    }

    #[test]
    fn test_not_in_combined() {
        let expr = parse_tokens(&["x", "not", "in", "y"]).unwrap();
        match expr {
            IfExpr::Infix { op: InfixOp::NotIn, .. } => {}
            _ => panic!("Expected not in"),
        }
    }

    #[test]
    fn test_is_not_combined() {
        let expr = parse_tokens(&["x", "is", "not", "y"]).unwrap();
        match expr {
            IfExpr::Infix { op: InfixOp::IsNot, .. } => {}
            _ => panic!("Expected is not"),
        }
    }

    #[test]
    fn test_precedence_and_or() {
        // "x or y and z" parses as "x or (y and z)" since and > or.
        let expr = parse_tokens(&["x", "or", "y", "and", "z"]).unwrap();
        match &expr {
            IfExpr::Infix { op: InfixOp::Or, right, .. } => {
                match right.as_ref() {
                    IfExpr::Infix { op: InfixOp::And, .. } => {}
                    _ => panic!("Expected and on right of or"),
                }
            }
            _ => panic!("Expected or at top"),
        }
    }

    #[test]
    fn test_comparison_operators() {
        for (op_str, expected_op) in [
            ("==", InfixOp::Eq),
            ("!=", InfixOp::NotEq),
            (">", InfixOp::Gt),
            (">=", InfixOp::Gte),
            ("<", InfixOp::Lt),
            ("<=", InfixOp::Lte),
        ] {
            let expr = parse_tokens(&["x", op_str, "y"]).unwrap();
            match expr {
                IfExpr::Infix { op, .. } => assert_eq!(op, expected_op, "Failed for {op_str}"),
                _ => panic!("Expected infix for {op_str}"),
            }
        }
    }

    #[test]
    fn test_error_on_trailing_token() {
        let result = parse_tokens(&["x", "y"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_error_on_empty() {
        let result = parse_tokens(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_error_infix_at_start() {
        let result = parse_tokens(&["and", "x"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_complex_expression() {
        // "not x == y and z in w" -> (not (x == y)) and (z in w).
        let expr = parse_tokens(&["not", "x", "==", "y", "and", "z", "in", "w"]).unwrap();
        match &expr {
            IfExpr::Infix { op: InfixOp::And, left, right } => {
                match left.as_ref() {
                    IfExpr::Prefix { op: PrefixOp::Not, operand } => {
                        match operand.as_ref() {
                            IfExpr::Infix { op: InfixOp::Eq, .. } => {}
                            _ => panic!("Expected == inside not"),
                        }
                    }
                    _ => panic!("Expected not prefix on left of and"),
                }
                match right.as_ref() {
                    IfExpr::Infix { op: InfixOp::In, .. } => {}
                    _ => panic!("Expected in on right of and"),
                }
            }
            _ => panic!("Expected and at top"),
        }
    }
}
