//! Aplikační vrstva Anvilu — use cases nad doménovými porty.
//!
//! Služby dostávají závislosti konstruktorem jako `Arc<dyn Port>`, takže jdou
//! testovat proti dvojníkům z modulu [`testing`] — bez načteného modelu,
//! bez disku a bez sítě.

pub mod chat;
pub mod compaction;
pub mod prompts;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use chat::{ChatService, SendOutcome, TurnContext};
pub use compaction::{CompactionPlan, CompactionService};
