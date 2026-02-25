//! Plugin/hook system.
//!
//! This is the architectural seam for future LLM integration.
//! An LLM plugin implements [`Hook`] and registers itself via [`HookRegistry`].
//! The runtime fires events at key points; registered hooks handle them.

pub mod events;
pub mod hook;
pub mod registry;

pub use events::HookEvent;
pub use hook::Hook;
pub use registry::HookRegistry;
