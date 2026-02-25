use shell_core::types::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // ── Words & literals ────────────────────────────────────────────────────
    /// A plain, unquoted word (e.g. `ls`, `foo`, `/usr/bin`)
    Word(String),
    /// Single-quoted string — no expansion performed.
    SingleQuoted(String),
    /// Double-quoted string — variable/command substitution allowed.
    DoubleQuoted(Vec<DqPart>),
    /// `$VAR` or `${VAR}` outside of quotes.
    Variable(String),
    /// `$(...)` command substitution.
    CmdSubst(String),

    // ── Operators ────────────────────────────────────────────────────────────
    /// `|`
    Pipe,
    /// `||`
    OrOr,
    /// `&&`
    AndAnd,
    /// `;`
    Semi,
    /// `;;`  (case statement)
    SemiSemi,
    /// `&`
    Ampersand,
    /// `>`
    Gt,
    /// `>>`
    GtGt,
    /// `<`
    Lt,
    /// `<(`  process substitution
    LtParen,
    /// `>(`  process substitution
    GtParen,
    /// `>&`
    GtAmpersand,
    /// `2>`  (or any fd redirect — stores the fd number)
    FdGt(u32),
    /// `2>>`
    FdGtGt(u32),

    // ── Grouping ─────────────────────────────────────────────────────────────
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,

    // ── Whitespace & structure ───────────────────────────────────────────────
    Newline,
    /// End of input.
    Eof,
}

/// A segment inside a double-quoted string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DqPart {
    Literal(String),
    Variable(String),
    CmdSubst(String),
}
