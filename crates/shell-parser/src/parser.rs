use thiserror::Error;

use shell_lexer::token::{DqPart, Token, TokenKind};

use crate::ast::{
    Command, List, ListItem, Pipeline, Redirect, RedirectKind, Separator, SimpleCommand, Word,
    WordPart,
};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("lex error: {0}")]
    Lex(String),
    #[error("unexpected token: {0}")]
    UnexpectedToken(String),
    #[error("unexpected end of input")]
    UnexpectedEof,
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    // ── Primitive helpers ────────────────────────────────────────────────────

    fn peek(&self) -> &TokenKind {
        self.tokens.get(self.pos).map(|t| &t.kind).unwrap_or(&TokenKind::Eof)
    }

    fn advance(&mut self) -> &TokenKind {
        let tok = &self.tokens[self.pos].kind;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), TokenKind::Newline) {
            self.advance();
        }
    }

    // ── Grammar rules ────────────────────────────────────────────────────────

    pub fn parse(&mut self) -> Result<List, ParseError> {
        let mut items = Vec::new();
        self.skip_newlines();

        while !matches!(self.peek(), TokenKind::Eof) {
            let pipeline = self.parse_pipeline()?;
            let separator = self.parse_separator();
            let is_eof = separator == Separator::Eof;
            items.push(ListItem { pipeline, separator });
            if is_eof {
                break;
            }
            self.skip_newlines();
        }

        Ok(List { items })
    }

    fn parse_separator(&mut self) -> Separator {
        match self.peek() {
            TokenKind::Semi | TokenKind::Newline => {
                self.advance();
                Separator::Sequential
            }
            TokenKind::Ampersand => {
                self.advance();
                Separator::Background
            }
            TokenKind::AndAnd => {
                self.advance();
                Separator::And
            }
            TokenKind::OrOr => {
                self.advance();
                Separator::Or
            }
            _ => Separator::Eof,
        }
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let negate = if matches!(self.peek(), TokenKind::Word(w) if w == "!") {
            self.advance();
            true
        } else {
            false
        };

        let mut commands = vec![self.parse_command()?];

        while matches!(self.peek(), TokenKind::Pipe) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }

        Ok(Pipeline { negate, commands })
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        // Only simple commands for now; compound commands come later.
        let mut words = Vec::new();
        let mut redirects = Vec::new();

        loop {
            match self.peek() {
                TokenKind::Eof
                | TokenKind::Newline
                | TokenKind::Semi
                | TokenKind::SemiSemi
                | TokenKind::Ampersand
                | TokenKind::AndAnd
                | TokenKind::OrOr
                | TokenKind::Pipe
                | TokenKind::RParen
                | TokenKind::RBrace => break,

                TokenKind::Gt
                | TokenKind::GtGt
                | TokenKind::Lt
                | TokenKind::GtAmpersand
                | TokenKind::FdGt(_)
                | TokenKind::FdGtGt(_) => {
                    redirects.push(self.parse_redirect()?);
                }

                _ => {
                    words.push(self.parse_word()?);
                }
            }
        }

        if words.is_empty() && redirects.is_empty() {
            return Err(ParseError::UnexpectedToken(format!("{:?}", self.peek())));
        }

        Ok(Command::Simple(SimpleCommand { words, redirects }))
    }

    fn parse_redirect(&mut self) -> Result<Redirect, ParseError> {
        let (fd, kind) = match self.advance().clone() {
            TokenKind::Gt => (1, RedirectKind::Output),
            TokenKind::GtGt => (1, RedirectKind::Append),
            TokenKind::Lt => (0, RedirectKind::Input),
            TokenKind::GtAmpersand => (1, RedirectKind::OutputDup),
            TokenKind::FdGt(fd) => (fd, RedirectKind::Output),
            TokenKind::FdGtGt(fd) => (fd, RedirectKind::Append),
            other => {
                return Err(ParseError::UnexpectedToken(format!("{other:?}")));
            }
        };
        let target = self.parse_word()?;
        Ok(Redirect { fd, kind, target })
    }

    fn parse_word(&mut self) -> Result<Word, ParseError> {
        let parts = match self.advance().clone() {
            TokenKind::Word(s) => vec![WordPart::Literal(s)],
            TokenKind::SingleQuoted(s) => vec![WordPart::Literal(s)],
            TokenKind::DoubleQuoted(dq_parts) => dq_parts
                .into_iter()
                .map(|p| match p {
                    DqPart::Literal(s) => WordPart::Literal(s),
                    DqPart::Variable(v) => WordPart::Variable(v),
                    DqPart::CmdSubst(_body) => {
                        // TODO: parse body recursively
                        WordPart::Literal(String::new())
                    }
                })
                .collect(),
            TokenKind::Variable(v) => vec![WordPart::Variable(v)],
            TokenKind::CmdSubst(_body) => {
                // TODO: parse body recursively
                vec![WordPart::Literal(String::new())]
            }
            other => {
                return Err(ParseError::UnexpectedToken(format!("{other:?}")));
            }
        };
        Ok(Word { parts })
    }
}
