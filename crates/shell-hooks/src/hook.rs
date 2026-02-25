use crate::events::HookEvent;

/// A plugin that observes and optionally modifies shell events.
///
/// Implement this trait to plug behaviour into the shell at any hook point.
/// The LLM integration will implement this trait — providing command
/// suggestions, error explanations, and intelligent completion.
pub trait Hook: Send + Sync {
    /// A stable identifier for this hook (used for logging and deregistration).
    fn name(&self) -> &str;

    /// Handle a shell event.
    ///
    /// Returns `Ok(Some(event))` to pass a (possibly modified) event to
    /// subsequent hooks, or `Ok(None)` to consume the event.
    fn on_event(&mut self, event: HookEvent) -> Result<Option<HookEvent>, Box<dyn std::error::Error>>;
}
