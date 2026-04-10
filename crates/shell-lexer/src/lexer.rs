use shell_core::types::Span;
use thiserror::Error;

use crate::token::{DqPart, Token, TokenKind};

#[derive(Debug, Error)]
pub enum LexError {
    #[error("unterminated single quote at byte {0}")]
    UnterminatedSingleQuote(usize),
    #[error("unterminated double quote at byte {0}")]
    UnterminatedDoubleQuote(usize),
    #[error("unexpected character {1:?} at byte {0}")]
    UnexpectedChar(usize, char),
}

/// Byte-level tokenizer over a UTF-8 input string.
pub struct Lexer<'src> {
    src: &'src str,
    pos: usize,
    done: bool,
}

impl<'src> Lexer<'src> {
    pub fn new(src: &'src str) -> Self {
        Self {
            src,
            pos: 0,
            done: false,
        }
    }

    // ── Primitive helpers ────────────────────────────────────────────────────

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    #[allow(dead_code)]
    fn peek2(&self) -> Option<char> {
        let mut chars = self.src[self.pos..].chars();
        chars.next();
        chars.next()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.advance();
        }
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(start, self.pos)
    }

    // ── Scanners ─────────────────────────────────────────────────────────────

    fn scan_word(&mut self, start: usize) -> TokenKind {
        loop {
            match self.peek() {
                None | Some(' ') | Some('\t') | Some('\n') | Some(';') | Some('|') | Some('&')
                | Some('<') | Some('>') | Some('(') | Some(')') => break,
                Some('\\') => {
                    self.advance(); // consume backslash
                    self.advance(); // consume escaped char
                }
                _ => {
                    self.advance();
                }
            }
        }
        TokenKind::Word(self.src[start..self.pos].to_owned())
    }

    fn scan_single_quoted(&mut self, start: usize) -> Result<TokenKind, LexError> {
        // opening ' already consumed
        let inner_start = self.pos;
        loop {
            match self.advance() {
                None => return Err(LexError::UnterminatedSingleQuote(start)),
                Some('\'') => {
                    let inner = self.src[inner_start..self.pos - 1].to_owned();
                    return Ok(TokenKind::SingleQuoted(inner));
                }
                _ => {}
            }
        }
    }

    fn scan_double_quoted(&mut self, start: usize) -> Result<TokenKind, LexError> {
        // opening " already consumed
        let mut parts: Vec<DqPart> = Vec::new();
        let mut literal = String::new();

        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedDoubleQuote(start)),
                Some('"') => {
                    self.advance();
                    if !literal.is_empty() {
                        parts.push(DqPart::Literal(std::mem::take(&mut literal)));
                    }
                    return Ok(TokenKind::DoubleQuoted(parts));
                }
                Some('$') => {
                    if !literal.is_empty() {
                        parts.push(DqPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.advance(); // consume '$'
                    match self.peek() {
                        Some('(') => {
                            self.advance();
                            let body = self.scan_until_close_paren();
                            parts.push(DqPart::CmdSubst(body));
                        }
                        Some('{') => {
                            self.advance();
                            let name = self.scan_until('}');
                            self.advance(); // consume '}'
                            parts.push(DqPart::Variable(name));
                        }
                        _ => {
                            let name = self.scan_varname();
                            parts.push(DqPart::Variable(name));
                        }
                    }
                }
                Some('\\') => {
                    self.advance();
                    if let Some(ch) = self.advance() {
                        literal.push(ch);
                    }
                }
                _ => {
                    literal.push(self.advance().unwrap());
                }
            }
        }
    }

    fn scan_varname(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.advance();
        }
        self.src[start..self.pos].to_owned()
    }

    fn scan_until(&mut self, stop: char) -> String {
        let start = self.pos;
        while self.peek().is_some() && self.peek() != Some(stop) {
            self.advance();
        }
        self.src[start..self.pos].to_owned()
    }

    fn scan_until_close_paren(&mut self) -> String {
        // Naive: counts nesting depth for balanced parens.
        let start = self.pos;
        let mut depth = 1usize;
        while let Some(ch) = self.advance() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return self.src[start..self.pos - 1].to_owned();
                    }
                }
                _ => {}
            }
        }
        self.src[start..self.pos].to_owned()
    }

    // ── Main scan step ───────────────────────────────────────────────────────

    fn next_token(&mut self) -> Option<Result<Token, LexError>> {
        self.skip_whitespace();

        let start = self.pos;

        let kind = match self.advance()? {
            '\n' => TokenKind::Newline,
            ';' if self.peek() == Some(';') => {
                self.advance();
                TokenKind::SemiSemi
            }
            ';' => TokenKind::Semi,
            '|' if self.peek() == Some('|') => {
                self.advance();
                TokenKind::OrOr
            }
            '|' => TokenKind::Pipe,
            '&' if self.peek() == Some('&') => {
                self.advance();
                TokenKind::AndAnd
            }
            '&' => TokenKind::Ampersand,
            '>' if self.peek() == Some('>') => {
                self.advance();
                TokenKind::GtGt
            }
            '>' if self.peek() == Some('&') => {
                self.advance();
                TokenKind::GtAmpersand
            }
            '>' => TokenKind::Gt,
            '<' => TokenKind::Lt,
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '\'' => match self.scan_single_quoted(start) {
                Ok(k) => k,
                Err(e) => return Some(Err(e)),
            },
            '"' => match self.scan_double_quoted(start) {
                Ok(k) => k,
                Err(e) => return Some(Err(e)),
            },
            '$' => match self.peek() {
                Some('(') => {
                    self.advance();
                    let body = self.scan_until_close_paren();
                    TokenKind::CmdSubst(body)
                }
                Some('{') => {
                    self.advance();
                    let name = self.scan_until('}');
                    self.advance();
                    TokenKind::Variable(name)
                }
                _ => TokenKind::Variable(self.scan_varname()),
            },
            '#' => {
                // Comment — consume to end of line
                while !matches!(self.peek(), None | Some('\n')) {
                    self.advance();
                }
                return self.next_token();
            }
            _ => self.scan_word(start),
        };

        Some(Ok(Token::new(kind, self.span_from(start))))
    }
}

impl<'src> Iterator for Lexer<'src> {
    type Item = Result<Token, LexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_token() {
            None => {
                self.done = true;
                let eof_span = Span::new(self.pos, self.pos);
                Some(Ok(Token::new(TokenKind::Eof, eof_span)))
            }
            item => item,
        }
    }
}
