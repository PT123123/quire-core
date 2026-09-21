// Rust Core — data structures for documents (M3+).
// See docs/ARCHITECTURE.md; nothing here may know about Slint types.

pub mod command;
pub mod database;
pub mod database_property;
// D3's drawing half of SPEC §三十九: the view's definition document, the
// columns it shows, and the shape one window of rows takes on its way to Slint.
pub mod database_view;
pub mod document;
pub mod embed;
pub mod highlight;
pub mod history;
pub mod icon;
pub mod math;
pub mod persistence;
pub mod types;

pub use command::{exec, plan, redo, undo, Command};
pub use document::Document;
pub use history::History;
pub use persistence::{Change, Repository, StorageError};
pub use types::{
    Attachment, AttachmentId, Block, BlockId, BlockKind, ColorKind, Lang, Mark, MarkKind, OrderKey,
    Page, PageFont, PageId, PersistedState,
};

