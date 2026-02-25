//! Word expansion: variables, tilde, glob, command substitution.

use shell_core::env::ShellEnv;

/// Errors that can occur during word expansion.
#[derive(Debug)]
pub struct ExpandError(pub String);

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Expand a raw word string into zero or more concrete strings.
///
/// Order of expansion:
/// 1. Tilde (`~` → `$HOME`)
/// 2. Variable (`$VAR`, `${VAR}`)
/// 3. Command substitution (`$(...)`)
/// 4. Pathname (glob) — returns multiple strings
/// 5. Word splitting
pub fn expand_word(word: &str, env: &ShellEnv) -> Result<Vec<String>, ExpandError> {
    let expanded = expand_tilde(word, env);
    let expanded = expand_vars(&expanded, env);
    // TODO: command substitution, glob expansion, word splitting
    Ok(vec![expanded])
}

fn expand_tilde(word: &str, env: &ShellEnv) -> String {
    if let Some(rest) = word.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            let home = env.get("HOME").unwrap_or("/");
            return format!("{home}{rest}");
        }
    }
    word.to_owned()
}

fn expand_vars(word: &str, env: &ShellEnv) -> String {
    let mut result = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '$' {
            result.push(ch);
            continue;
        }

        match chars.peek() {
            Some('{') => {
                chars.next(); // consume '{'
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                result.push_str(env.get(&name).unwrap_or(""));
            }
            Some(c) if c.is_alphanumeric() || *c == '_' => {
                let name: String = std::iter::from_fn(|| {
                    chars.peek().filter(|&&c| c.is_alphanumeric() || c == '_')?;
                    chars.next()
                })
                .collect();
                result.push_str(env.get(&name).unwrap_or(""));
            }
            Some('?') => {
                chars.next();
                // $? is the last exit status — caller must pre-set it in env
                result.push_str(env.get("?").unwrap_or("0"));
            }
            _ => result.push('$'),
        }
    }

    result
}
