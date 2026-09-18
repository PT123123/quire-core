// Rust Core — data structures for documents (M3+).
// See docs/ARCHITECTURE.md; nothing here may know about Slint types.

pub mod persistence;
pub mod types;

pub use persistence::{Change, Repository, StorageError};
pub use types::{Block, BlockId, BlockKind, OrderKey, Page, PageId, PersistedState};
