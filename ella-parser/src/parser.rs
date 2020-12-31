use crate::ast::Expr;
use crate::lexer::Token;
use ella_source::{Source, SyntaxError};
use logos::{Lexer, Logos};
use std::mem;

mod expr;
pub use expr::*;

pub struct Parser<'a> {
    /// Cached token for peeking.
    current_token: Token,
    lexer: Lexer<'a, Token>,
    /// Source code
    source: &'a Source<'a>,
}

impl<'a> Parser<'a> {
    pub fn new(source: &'a Source<'a>) -> Self {
        let mut lexer = Token::lexer(source.content);
        Self {
            current_token: lexer.next().unwrap(),
            lexer,
            source,
        }
    }
}

impl<'a> Parser<'a> {
    pub fn parse_program(&mut self) -> Expr {
        self.parse_expr()
    }
}

/// Parse utilities
impl<'a> Parser<'a> {
    fn next(&mut self) -> Token {
        let token = self.lexer.next().unwrap_or(Token::Eof);
        self.current_token = token.clone();
        token
    }

    /// Predicate that tests whether the next token has the same discriminant and eats the next token if yes as a side effect.
    #[must_use = "to unconditionally eat a token, use Self::next"]
    fn eat(&mut self, tok: Token) -> bool {
        if mem::discriminant(&self.current_token) == mem::discriminant(&tok) {
            self.next(); // eat token
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: Token) {
        if !self.eat(tok) {
            self.unexpected()
        }
    }

    /// Raises an unexpected token error.
    fn unexpected(&mut self) {
        self.source
            .errors
            .add_error(SyntaxError::new("Unexpected token", self.lexer.span()))
    }
}