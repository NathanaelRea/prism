use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ConditionExpr {
    Bool(bool),
    Number(i64),
    String(String),
    Reference(String),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Equal(Box<Self>, Box<Self>),
    NotEqual(Box<Self>, Box<Self>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "availability", content = "value", rename_all = "snake_case")]
pub enum ConditionValue {
    Known(serde_json::Value),
    Missing,
    Stale,
    Unsupported,
    Unavailable,
    Unknown,
}

impl ConditionExpr {
    pub fn parse(source: &str) -> Result<Self, ConditionError> {
        let mut parser = Parser::new(source)?;
        let expression = parser.parse_or()?;
        if parser.peek() != &Token::End {
            return Err(parser.error("unexpected token after expression"));
        }
        Ok(expression)
    }

    pub fn references(&self, output: &mut Vec<String>) {
        match self {
            Self::Reference(reference) => output.push(reference.clone()),
            Self::Not(value) => value.references(output),
            Self::And(left, right)
            | Self::Or(left, right)
            | Self::Equal(left, right)
            | Self::NotEqual(left, right) => {
                left.references(output);
                right.references(output);
            }
            Self::Bool(_) | Self::Number(_) | Self::String(_) => {}
        }
    }

    /// Evaluate only captured values. Non-known availability propagates without becoming false.
    pub fn evaluate(&self, values: &BTreeMap<String, ConditionValue>) -> ConditionValue {
        use serde_json::Value;
        match self {
            Self::Bool(value) => ConditionValue::Known(Value::Bool(*value)),
            Self::Number(value) => ConditionValue::Known(Value::Number((*value).into())),
            Self::String(value) => ConditionValue::Known(Value::String(value.clone())),
            Self::Reference(reference) => values
                .get(reference)
                .cloned()
                .unwrap_or(ConditionValue::Missing),
            Self::Not(value) => match value.evaluate(values) {
                ConditionValue::Known(Value::Bool(value)) => {
                    ConditionValue::Known(Value::Bool(!value))
                }
                other => non_known(other),
            },
            Self::And(left, right) => boolean_binary(left, right, values, |a, b| a && b),
            Self::Or(left, right) => boolean_binary(left, right, values, |a, b| a || b),
            Self::Equal(left, right) => equality(left, right, values, false),
            Self::NotEqual(left, right) => equality(left, right, values, true),
        }
    }
}

fn boolean_binary(
    left: &ConditionExpr,
    right: &ConditionExpr,
    values: &BTreeMap<String, ConditionValue>,
    operation: impl FnOnce(bool, bool) -> bool,
) -> ConditionValue {
    match (left.evaluate(values), right.evaluate(values)) {
        (
            ConditionValue::Known(serde_json::Value::Bool(left)),
            ConditionValue::Known(serde_json::Value::Bool(right)),
        ) => ConditionValue::Known(serde_json::Value::Bool(operation(left, right))),
        (left, right) => propagate(left, right),
    }
}

fn equality(
    left: &ConditionExpr,
    right: &ConditionExpr,
    values: &BTreeMap<String, ConditionValue>,
    negate: bool,
) -> ConditionValue {
    match (left.evaluate(values), right.evaluate(values)) {
        (ConditionValue::Known(left), ConditionValue::Known(right)) => {
            ConditionValue::Known(serde_json::Value::Bool((left == right) != negate))
        }
        (left, right) => propagate(left, right),
    }
}

fn propagate(left: ConditionValue, right: ConditionValue) -> ConditionValue {
    if !matches!(left, ConditionValue::Known(_)) {
        non_known(left)
    } else {
        non_known(right)
    }
}

fn non_known(value: ConditionValue) -> ConditionValue {
    match value {
        ConditionValue::Known(_) => ConditionValue::Unknown,
        value => value,
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Bool(bool),
    Number(i64),
    String(String),
    Reference(String),
    Not,
    And,
    Or,
    Equal,
    NotEqual,
    Open,
    Close,
    End,
}

struct Parser {
    tokens: Vec<(Token, usize)>,
    cursor: usize,
}

impl Parser {
    fn new(source: &str) -> Result<Self, ConditionError> {
        Ok(Self {
            tokens: tokenize(source)?,
            cursor: 0,
        })
    }

    fn parse_or(&mut self) -> Result<ConditionExpr, ConditionError> {
        let mut value = self.parse_and()?;
        while self.peek() == &Token::Or {
            self.advance();
            value = ConditionExpr::Or(Box::new(value), Box::new(self.parse_and()?));
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> Result<ConditionExpr, ConditionError> {
        let mut value = self.parse_equality()?;
        while self.peek() == &Token::And {
            self.advance();
            value = ConditionExpr::And(Box::new(value), Box::new(self.parse_equality()?));
        }
        Ok(value)
    }

    fn parse_equality(&mut self) -> Result<ConditionExpr, ConditionError> {
        let mut value = self.parse_unary()?;
        loop {
            let constructor = match self.peek() {
                Token::Equal => ConditionExpr::Equal,
                Token::NotEqual => ConditionExpr::NotEqual,
                _ => break,
            };
            self.advance();
            value = constructor(Box::new(value), Box::new(self.parse_unary()?));
        }
        Ok(value)
    }

    fn parse_unary(&mut self) -> Result<ConditionExpr, ConditionError> {
        if self.peek() == &Token::Not {
            self.advance();
            return Ok(ConditionExpr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<ConditionExpr, ConditionError> {
        let token = self.peek().clone();
        self.advance();
        match token {
            Token::Bool(value) => Ok(ConditionExpr::Bool(value)),
            Token::Number(value) => Ok(ConditionExpr::Number(value)),
            Token::String(value) => Ok(ConditionExpr::String(value)),
            Token::Reference(value) => Ok(ConditionExpr::Reference(value)),
            Token::Open => {
                let value = self.parse_or()?;
                if self.peek() != &Token::Close {
                    return Err(self.error("missing ')'"));
                }
                self.advance();
                Ok(value)
            }
            _ => Err(self.error("expected a value, reference, or parenthesized expression")),
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.cursor].0
    }

    fn advance(&mut self) {
        self.cursor = (self.cursor + 1).min(self.tokens.len() - 1);
    }

    fn error(&self, message: &str) -> ConditionError {
        ConditionError {
            offset: self.tokens[self.cursor].1,
            message: message.into(),
        }
    }
}

fn tokenize(source: &str) -> Result<Vec<(Token, usize)>, ConditionError> {
    let bytes = source.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        let start = cursor;
        let token = match bytes[cursor] {
            b'(' => {
                cursor += 1;
                Token::Open
            }
            b')' => {
                cursor += 1;
                Token::Close
            }
            b'!' if bytes.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                Token::NotEqual
            }
            b'!' => {
                cursor += 1;
                Token::Not
            }
            b'=' if bytes.get(cursor + 1) == Some(&b'=') => {
                cursor += 2;
                Token::Equal
            }
            b'&' if bytes.get(cursor + 1) == Some(&b'&') => {
                cursor += 2;
                Token::And
            }
            b'|' if bytes.get(cursor + 1) == Some(&b'|') => {
                cursor += 2;
                Token::Or
            }
            b'"' | b'\'' => {
                let quote = bytes[cursor];
                cursor += 1;
                let content = cursor;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += 1;
                }
                if cursor == bytes.len() {
                    return Err(ConditionError {
                        offset: start,
                        message: "unterminated string".into(),
                    });
                }
                let value = source[content..cursor].to_owned();
                cursor += 1;
                Token::String(value)
            }
            byte if byte.is_ascii_digit() || byte == b'-' => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                Token::Number(source[start..cursor].parse().map_err(|_| ConditionError {
                    offset: start,
                    message: "invalid integer".into(),
                })?)
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                cursor += 1;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric()
                        || matches!(bytes[cursor], b'_' | b'-' | b'.'))
                {
                    cursor += 1;
                }
                match &source[start..cursor] {
                    "true" => Token::Bool(true),
                    "false" => Token::Bool(false),
                    value => Token::Reference(value.to_owned()),
                }
            }
            _ => {
                return Err(ConditionError {
                    offset: start,
                    message: "invalid condition token".into(),
                });
            }
        };
        output.push((token, start));
    }
    output.push((Token::End, source.len()));
    Ok(output)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ConditionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "condition at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for ConditionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_precedence_and_references() {
        let parsed =
            ConditionExpr::parse("inputs.ready && !steps.check.outputs.failed == true").unwrap();
        let mut references = Vec::new();
        parsed.references(&mut references);
        assert_eq!(references, ["inputs.ready", "steps.check.outputs.failed"]);
        assert!(matches!(parsed, ConditionExpr::And(_, _)));
    }

    #[test]
    fn unavailable_values_are_not_collapsed_to_false() {
        let expression = ConditionExpr::parse("inputs.ready == true").unwrap();
        for unavailable in [
            ConditionValue::Missing,
            ConditionValue::Stale,
            ConditionValue::Unsupported,
            ConditionValue::Unavailable,
            ConditionValue::Unknown,
        ] {
            assert_eq!(
                expression.evaluate(&BTreeMap::from([(
                    "inputs.ready".into(),
                    unavailable.clone()
                )])),
                unavailable
            );
        }
    }
}
