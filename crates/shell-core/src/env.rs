use std::collections::HashMap;

/// Shell environment: variables, exported vars, and shell options.
#[derive(Debug, Default, Clone)]
pub struct ShellEnv {
    vars: HashMap<String, String>,
    exported: HashMap<String, String>,
}

impl ShellEnv {
    pub fn new() -> Self {
        Self::default()
    }

    /// Populate from the process environment.
    pub fn from_process_env() -> Self {
        let exported: HashMap<String, String> = std::env::vars().collect();
        Self { vars: HashMap::new(), exported }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .or_else(|| self.exported.get(key))
            .map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    pub fn export(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let k = key.into();
        let v = value.into();
        self.vars.remove(&k);
        self.exported.insert(k, v);
    }

    pub fn unset(&mut self, key: &str) {
        self.vars.remove(key);
        self.exported.remove(key);
    }

    pub fn is_exported(&self, key: &str) -> bool {
        self.exported.contains_key(key)
    }

    /// Iterate over all exported variables (for passing to child processes).
    pub fn exported_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.exported.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Iterate all variables (local + exported).
    pub fn all_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .chain(self.exported.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }
}
