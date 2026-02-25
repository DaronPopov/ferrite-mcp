use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("command not found: {0}")]
    CommandNotFound(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("no such file or directory: {0}")]
    NoSuchFile(String),

    #[error("lex error: {0}")]
    Lex(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
