pub mod ast;
pub mod parser;

pub use ast::{Command, List, Pipeline, Redirect, RedirectKind, WordPart};
pub use parser::{ParseError, Parser};

/// Convenience: lex + parse an input string into a command list.
pub fn parse(input: &str) -> Result<List, ParseError> {
    use shell_lexer::lex;
    let tokens = lex(input).map_err(|e| ParseError::Lex(e.to_string()))?;
    Parser::new(tokens).parse()
}
