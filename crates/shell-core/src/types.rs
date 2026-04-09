/// Byte-offset range in the original source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A shell word paired with its source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub value: String,
    pub span: Span,
}

impl Word {
    pub fn new(value: impl Into<String>, span: Span) -> Self {
        Self {
            value: value.into(),
            span,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for Word {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

/// The exit status of a command or pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    pub const SUCCESS: Self = Self(0);

    pub fn success(self) -> bool {
        self.0 == 0
    }

    pub fn code(self) -> i32 {
        self.0
    }
}

impl From<i32> for ExitStatus {
    fn from(code: i32) -> Self {
        Self(code)
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
