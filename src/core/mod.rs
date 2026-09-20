// Rust Core — data structures for documents (M3+).
// See docs/ARCHITECTURE.md; nothing here may know about Slint types.

pub mod command;
pub mod document;
pub mod history;
pub mod persistence;
pub mod types;

pub use command::{exec, plan, redo, undo, Command};
pub use document::Document;
pub use history::History;
pub use persistence::{Change, Repository, StorageError};
pub use types::{
    Attachment, AttachmentId, Block, BlockId, BlockKind, ColorKind, Mark, MarkKind, OrderKey, Page,
    PageId, PersistedState,
};

