pub mod lexer;
pub mod token;

pub use lexer::{LexError, Lexer};
pub use token::{Token, TokenKind};

/// Convenience: lex an entire input string into a token list.
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(input).collect()
}
