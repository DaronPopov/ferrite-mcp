//! Abstract Syntax Tree for the shell grammar.
//!
//! Grammar sketch (simplified):
//!
//! ```text
//! list       := pipeline (list_op pipeline)*
//! list_op    := '&&' | '||' | ';' | '&' | '\n'
//! pipeline   := ['!'] command ('|' command)*
//! command    := simple_cmd | compound_cmd
//! simple_cmd := word+ redirect*
//! redirect   := [fd] redir_op word
//! word       := word_part+
//! word_part  := Literal | Variable | CmdSubst | Glob
//! ```

/// A complete command list (the root node).
#[derive(Debug, Clone)]
pub struct List {
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone)]
pub struct ListItem {
    pub pipeline: Pipeline,
    pub separator: Separator,
}

/// How a list item is terminated / joined to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    /// `;` or `\n` — sequential foreground execution
    Sequential,
    /// `&` — background execution
    Background,
    /// `&&` — run next only if this succeeded
    And,
    /// `||` — run next only if this failed
    Or,
    /// End of input
    Eof,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    /// If true, the exit status is logically negated (`!`).
    pub negate: bool,
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Simple(SimpleCommand),
    // Future: Compound(CompoundCommand) — if/for/while/case
}

#[derive(Debug, Clone)]
pub struct SimpleCommand {
    /// The command name and its arguments, each as a word.
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

/// A shell word composed of one or more parts (literal text, variables, etc.)
#[derive(Debug, Clone)]
pub struct Word {
    pub parts: Vec<WordPart>,
}

impl Word {
    pub fn literal(s: impl Into<String>) -> Self {
        Self {
            parts: vec![WordPart::Literal(s.into())],
        }
    }
}

#[derive(Debug, Clone)]
pub enum WordPart {
    /// Plain literal text.
    Literal(String),
    /// `$VAR` or `${VAR}`.
    Variable(String),
    /// `$(...)` command substitution.
    CmdSubst(Box<List>),
    /// A glob pattern segment (`*`, `?`, `[...]`).
    Glob(String),
}

#[derive(Debug, Clone)]
pub struct Redirect {
    /// Source file descriptor (defaults to 0 for `<`, 1 for `>`).
    pub fd: u32,
    pub kind: RedirectKind,
    pub target: Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    /// `>`
    Output,
    /// `>>`
    Append,
    /// `<`
    Input,
    /// `>&`
    OutputDup,
}
