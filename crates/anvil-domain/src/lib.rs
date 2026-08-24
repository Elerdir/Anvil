//! Doménové jádro Anvilu.
//!
//! Obsahuje entity, hodnotové objekty a **porty** — traity popisující, co
//! aplikace od okolí potřebuje. Žádná I/O závislost: nic tu nesahá na disk,
//! na síť ani na databázi, takže je celá vrstva testovatelná bez prostředí.
//!
//! Závislosti jdou jedním směrem: `src-tauri` → `anvil-application` →
//! `anvil-domain` ← `anvil-infrastructure`. Doména nesmí importovat nic
//! z vnějších vrstev.

pub mod conversation;
pub mod error;
pub mod history;
pub mod id;
pub mod model;
pub mod ports;
pub mod review;
pub mod settings;
pub mod tool;
pub mod workspace;

pub use conversation::{Conversation, Message, Role};
pub use error::{DomainError, DomainResult};
pub use history::ConversationSummary;
pub use id::{ConversationId, MessageId};
pub use model::{
    ChatTemplateKind, InferenceSettings, InstalledModel, ModelId, ModelRole, ModelSpec, Sampling,
};
pub use review::{Finding, ReviewReport, Severity};
pub use settings::{AppSettings, RoleModels};
pub use tool::{ParamKind, ToolCall, ToolParam, ToolResult, ToolSpec};
pub use workspace::{RelativePath, Workspace};
