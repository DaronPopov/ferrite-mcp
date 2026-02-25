use crate::{events::HookEvent, hook::Hook};

/// Ordered registry of hooks. Events are dispatched in registration order.
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<Box<dyn Hook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook. Hooks fire in the order they are registered.
    pub fn register(&mut self, hook: Box<dyn Hook>) {
        self.hooks.push(hook);
    }

    /// Deregister a hook by name.
    pub fn deregister(&mut self, name: &str) {
        self.hooks.retain(|h| h.name() != name);
    }

    /// Fire an event through all registered hooks.
    ///
    /// Each hook may transform or consume the event. If a hook returns `None`
    /// the event is consumed and no further hooks are called.
    pub fn fire(&mut self, event: HookEvent) {
        let mut current = event;
        for hook in &mut self.hooks {
            match hook.on_event(current) {
                Ok(Some(next)) => current = next,
                Ok(None) => return,
                Err(e) => {
                    // Hooks must not crash the shell. Log and continue.
                    eprintln!("[hook:{}] error: {e}", hook.name());
                    return;
                }
            }
        }
    }
}
