//! Parser for debugger expressions (watch, hover, REPL and breakpoint conditions).
//!
//! The grammar is a C-like subset: literals, identifiers (including `$name`
//! WebAssembly references and `a::b` qualified names), unary and binary
//! operators with C precedence, member access, indexing and function calls.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
    Deref,
    AddressOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    And,
    Or,
}

impl BinaryOp {
    fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => 1,
            BinaryOp::And => 2,
            BinaryOp::BitOr => 3,
            BinaryOp::BitXor => 4,
            BinaryOp::BitAnd => 5,
            BinaryOp::Eq | BinaryOp::Ne => 6,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => 7,
            BinaryOp::Shl | BinaryOp::Shr => 8,
            BinaryOp::Add | BinaryOp::Sub => 9,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Integer(i128),
    Float(f64),
    String(String),
    Char(char),
    Bool(bool),
    Identifier(String),
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Member {
        expr: Box<Expr>,
        field: String,
        arrow: bool,
    },
    Index {
        expr: Box<Expr>,
        index: Box<Expr>,
    },
    /// `receiver.method(args)`, limited to built-in methods such as `len()`.
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
}

/// Limits that keep parsing and the recursive evaluation within a bounded stack depth.
const MAX_LENGTH: usize = 1024;
const MAX_DEPTH: usize = 100;

pub fn parse(input: &str) -> Result<Expr> {
    if input.len() > MAX_LENGTH {
        return Err(Error::Eval(format!(
            "expressions longer than {MAX_LENGTH} bytes are not supported"
        )));
    }
    let mut parser = Parser::new(input);
    let expr = parser.parse_binary(0)?;
    parser.skip_whitespace();
    if !parser.is_eof() {
        return Err(Error::Eval(format!(
            "unexpected input at position {}",
            parser.pos
        )));
    }
    if depth(&expr) > MAX_DEPTH {
        return Err(Error::Eval("expression is nested too deeply".into()));
    }
    Ok(expr)
}

/// Height of an expression tree, computed without recursion.
fn depth(expr: &Expr) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(expr, 1)];
    while let Some((expr, level)) = pending.pop() {
        deepest = deepest.max(level);
        let children: Vec<&Expr> = match expr {
            Expr::Unary { expr, .. } | Expr::Member { expr, .. } => vec![expr],
            Expr::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            Expr::Index { expr, index } => vec![expr, index],
            Expr::Call { args, .. } => args.iter().collect(),
            Expr::MethodCall { receiver, args, .. } => {
                std::iter::once(&**receiver).chain(args).collect()
            }
            _ => Vec::new(),
        };
        pending.extend(children.into_iter().map(|child| (child, level + 1)));
    }
    deepest
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn error(&self, message: &str) -> Error {
        Error::Eval(format!("{message} at position {}", self.pos))
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_whitespace();
        if self.input[self.pos..].starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn peek_binary_op(&mut self) -> Option<(BinaryOp, usize)> {
        self.skip_whitespace();
        let rest = &self.input[self.pos..];
        const OPS: &[(&str, BinaryOp)] = &[
            ("||", BinaryOp::Or),
            ("&&", BinaryOp::And),
            ("==", BinaryOp::Eq),
            ("!=", BinaryOp::Ne),
            ("<=", BinaryOp::Le),
            (">=", BinaryOp::Ge),
            ("<<", BinaryOp::Shl),
            (">>", BinaryOp::Shr),
            ("<", BinaryOp::Lt),
            (">", BinaryOp::Gt),
            ("+", BinaryOp::Add),
            ("-", BinaryOp::Sub),
            ("*", BinaryOp::Mul),
            ("/", BinaryOp::Div),
            ("%", BinaryOp::Rem),
            ("&", BinaryOp::BitAnd),
            ("^", BinaryOp::BitXor),
            ("|", BinaryOp::BitOr),
        ];
        OPS.iter()
            .find(|(text, _)| rest.starts_with(text) && !(*text == "-" && rest.starts_with("->")))
            .map(|(text, op)| (*op, text.len()))
    }

    fn parse_binary(&mut self, min_precedence: u8) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        while let Some((op, len)) = self.peek_binary_op() {
            let precedence = op.precedence();
            if precedence <= min_precedence {
                break;
            }
            self.pos += len;
            let rhs = self.parse_binary(precedence)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// Every nested operand passes through here, so this bounds the recursion.
    fn parse_unary(&mut self) -> Result<Expr> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("expression is nested too deeply"));
        }
        self.depth += 1;
        let expr = self.parse_prefixed();
        self.depth -= 1;
        expr
    }

    fn parse_prefixed(&mut self) -> Result<Expr> {
        self.skip_whitespace();
        let op = match self.peek() {
            Some(b'-') if !self.peek_at(1).is_some_and(|c| c == b'>') => Some(UnaryOp::Neg),
            Some(b'!') if self.peek_at(1) != Some(b'=') => Some(UnaryOp::Not),
            Some(b'~') => Some(UnaryOp::BitNot),
            Some(b'*') => Some(UnaryOp::Deref),
            Some(b'&') if self.peek_at(1) != Some(b'&') => Some(UnaryOp::AddressOf),
            _ => None,
        };
        let Some(op) = op else {
            return self.parse_postfix();
        };
        self.pos += 1;
        let operand = self.parse_unary()?;
        Ok(match (op, operand) {
            (UnaryOp::Neg, Expr::Integer(v)) => Expr::Integer(-v),
            (UnaryOp::Neg, Expr::Float(v)) => Expr::Float(-v),
            (op, operand) => Expr::Unary {
                op,
                expr: Box::new(operand),
            },
        })
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            self.skip_whitespace();
            if self.eat("->") {
                let field = self.parse_field()?;
                expr = Expr::Member {
                    expr: Box::new(expr),
                    field,
                    arrow: true,
                };
            } else if self.peek() == Some(b'.') {
                self.pos += 1;
                let field = self.parse_field()?;
                if self.eat("(") {
                    let args = self.parse_arguments()?;
                    expr = Expr::MethodCall {
                        receiver: Box::new(expr),
                        method: field,
                        args,
                    };
                    continue;
                }
                expr = Expr::Member {
                    expr: Box::new(expr),
                    field,
                    arrow: false,
                };
            } else if self.peek() == Some(b'[') {
                self.pos += 1;
                let index = self.parse_binary(0)?;
                if !self.eat("]") {
                    return Err(self.error("expected ']'"));
                }
                expr = Expr::Index {
                    expr: Box::new(expr),
                    index: Box::new(index),
                };
            } else {
                return Ok(expr);
            }
        }
    }

    fn parse_field(&mut self) -> Result<String> {
        self.skip_whitespace();
        let start = self.pos;
        while self.peek().is_some_and(is_ident_char) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(self.error("expected a field name"));
        }
        Ok(self.input[start..self.pos].to_string())
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'0'..=b'9') => self.parse_number(),
            Some(b'.') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {
                self.parse_number()
            }
            Some(b'"') => self.parse_string(b'"'),
            Some(b'\'') => self.parse_string(b'\''),
            Some(b'(') => {
                self.pos += 1;
                let inner = self.parse_binary(0)?;
                if !self.eat(")") {
                    return Err(self.error("expected ')'"));
                }
                Ok(inner)
            }
            Some(c) if is_ident_start(c) => self.parse_identifier_or_call(),
            Some(c) => Err(self.error(&format!("unexpected character '{}'", c as char))),
            None => Err(Error::Eval("unexpected end of input".to_string())),
        }
    }

    fn parse_number(&mut self) -> Result<Expr> {
        let start = self.pos;
        let rest = &self.input[self.pos..];
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with("0x") || lower.starts_with("0b") || lower.starts_with("0o") {
            self.pos += 2;
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_hexdigit() || c == b'_')
            {
                self.pos += 1;
            }
            let text = &self.input[start..self.pos];
            return crate::runtime::values::parse_int(text)
                .map(Expr::Integer)
                .ok_or_else(|| Error::Eval(format!("invalid integer literal: {text}")));
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'_' {
                self.pos += 1;
            } else if c == b'.' && !is_float && self.peek_at(1).is_none_or(|n| n.is_ascii_digit()) {
                is_float = true;
                self.pos += 1;
            } else if (c == b'e' || c == b'E')
                && self
                    .peek_at(1)
                    .is_some_and(|n| n.is_ascii_digit() || n == b'-' || n == b'+')
            {
                is_float = true;
                self.pos += 2;
            } else {
                break;
            }
        }
        let text = self.input[start..self.pos].replace('_', "");
        if is_float {
            text.parse::<f64>()
                .map(Expr::Float)
                .map_err(|_| Error::Eval(format!("invalid float literal: {text}")))
        } else {
            text.parse::<i128>()
                .map(Expr::Integer)
                .map_err(|_| Error::Eval(format!("invalid integer literal: {text}")))
        }
    }

    fn parse_string(&mut self, quote: u8) -> Result<Expr> {
        self.pos += 1;
        let mut value = String::new();
        let mut chars = self.input[self.pos..].char_indices();
        while let Some((i, c)) = chars.next() {
            if c as u32 == u32::from(quote) {
                self.pos += i + 1;
                if quote == b'\'' {
                    let mut it = value.chars();
                    if let (Some(single), None) = (it.next(), it.next()) {
                        return Ok(Expr::Char(single));
                    }
                }
                return Ok(Expr::String(value));
            }
            if c == '\\' {
                let Some((_, escaped)) = chars.next() else {
                    break;
                };
                value.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '0' => '\0',
                    other => other,
                });
            } else {
                value.push(c);
            }
        }
        Err(Error::Eval("unterminated string literal".to_string()))
    }

    fn parse_identifier_or_call(&mut self) -> Result<Expr> {
        let start = self.pos;
        loop {
            while self.peek().is_some_and(is_ident_char) {
                self.pos += 1;
            }
            if self.peek() == Some(b':')
                && self.peek_at(1) == Some(b':')
                && self.peek_at(2).is_some_and(is_ident_start)
            {
                self.pos += 2;
                continue;
            }
            break;
        }
        let name = self.input[start..self.pos].to_string();
        match name.as_str() {
            "true" => return Ok(Expr::Bool(true)),
            "false" => return Ok(Expr::Bool(false)),
            _ => {}
        }
        self.skip_whitespace();
        if self.peek() != Some(b'(') {
            return Ok(Expr::Identifier(name));
        }
        self.pos += 1;
        let args = self.parse_arguments()?;
        Ok(Expr::Call { name, args })
    }

    /// Parses arguments after an opening parenthesis, through the closing one.
    fn parse_arguments(&mut self) -> Result<Vec<Expr>> {
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            args.push(self.parse_binary(0)?);
            if self.eat(",") {
                continue;
            }
            if self.eat(")") {
                return Ok(args);
            }
            return Err(self.error("expected ',' or ')' in argument list"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    #[test]
    fn precedence_follows_c() {
        assert_eq!(
            parse("1 + 2 * 3").unwrap(),
            bin(
                BinaryOp::Add,
                Expr::Integer(1),
                bin(BinaryOp::Mul, Expr::Integer(2), Expr::Integer(3))
            )
        );
        assert_eq!(
            parse("a == 1 && b < 2").unwrap(),
            bin(
                BinaryOp::And,
                bin(BinaryOp::Eq, Expr::Identifier("a".into()), Expr::Integer(1)),
                bin(BinaryOp::Lt, Expr::Identifier("b".into()), Expr::Integer(2))
            )
        );
        assert_eq!(
            parse("10 - 4 - 3").unwrap(),
            bin(
                BinaryOp::Sub,
                bin(BinaryOp::Sub, Expr::Integer(10), Expr::Integer(4)),
                Expr::Integer(3)
            )
        );
    }

    #[test]
    fn postfix_chains() {
        let expr = parse("points[1].x").unwrap();
        assert_eq!(
            expr,
            Expr::Member {
                expr: Box::new(Expr::Index {
                    expr: Box::new(Expr::Identifier("points".into())),
                    index: Box::new(Expr::Integer(1)),
                }),
                field: "x".into(),
                arrow: false,
            }
        );
        assert!(matches!(
            parse("p->next").unwrap(),
            Expr::Member { arrow: true, .. }
        ));
        assert!(matches!(
            parse("*ptr").unwrap(),
            Expr::Unary {
                op: UnaryOp::Deref,
                ..
            }
        ));
        assert!(matches!(
            parse("word.len() > 7").unwrap(),
            Expr::Binary {
                op: BinaryOp::Gt,
                ..
            }
        ));
        assert!(matches!(
            parse("v.is_empty()").unwrap(),
            Expr::MethodCall { .. }
        ));
    }

    #[test]
    fn literals_and_identifiers() {
        assert_eq!(parse("0x10").unwrap(), Expr::Integer(16));
        assert_eq!(parse("'a'").unwrap(), Expr::Char('a'));
        assert_eq!(parse("true").unwrap(), Expr::Bool(true));
        assert_eq!(
            parse("$local3").unwrap(),
            Expr::Identifier("$local3".into())
        );
        assert_eq!(
            parse("arith::COUNTER").unwrap(),
            Expr::Identifier("arith::COUNTER".into())
        );
        assert_eq!(parse("1e3").unwrap(), Expr::Float(1000.0));
        assert!(parse("1 +").is_err());
        assert!(parse("(1").is_err());
    }
}
