// Persistence contract: what the M3 storage layer must provide to the rest
// of the app. Core defines it; `src/storage/` implements it (SQLite, M3).
//
// Model: the app holds the full in-memory state and emits ordered change
// lists (ADR-0012). `apply` is the only write path and must be one
// transaction per call — an intermediate state may never be observable
// (SPEC §十八). `load` and `replace_all` are the bulk paths (startup and
// checkpoint/repair).

use std::fmt;

use super::types::{Block, BlockId, BlockKind, OrderKey, Page, PageId, PersistedState};

/// Ordered list of persisted mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    PageCreated(Page),
    PageTitleSet { id: PageId, title: String },
    PageMoved { id: PageId, parent: Option<PageId>, order: OrderKey },
    PageFavoriteSet { id: PageId, favorite: bool },
    PageExpandedSet { id: PageId, expanded: bool },
    /// Storage deletes the page and (recursively) its sub-pages.
    PageDeleted { id: PageId },

    BlockInserted(Block),
    BlockTextSet { id: BlockId, text: String },
    BlockKindSet { id: BlockId, kind: BlockKind },
    BlockCheckedSet { id: BlockId, checked: bool },
    BlockMoved { id: BlockId, parent: Option<BlockId>, order: OrderKey },
    /// Storage deletes the block and (recursively) its children; undo
    /// replays the captured subtree as `BlockInserted`s.
    BlockDeleted { id: BlockId },

    MetaSet { key: String, value: String },
    SettingSet { key: String, value: String },
}

/// Errors surfaced by a `Repository` implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// Could not open/create the database file.
    Open(String),
    /// SQL failure while reading or writing.
    Sql(String),
    /// Opened fine but the content failed the integrity/schema check.
    Corrupt(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Open(s) => write!(f, "storage open failed: {s}"),
            StorageError::Sql(s) => write!(f, "storage sql error: {s}"),
            StorageError::Corrupt(s) => write!(f, "storage corrupt: {s}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// The one seam between the app and SQLite (M3). Implementations must be
/// usable from any thread (the debounced flush may leave the UI thread).
pub trait Repository: Send + Sync {
    /// Load the full persisted state at startup (after integrity checks).
    fn load(&self) -> Result<PersistedState, StorageError>;

    /// Apply an ordered change list atomically: all of it, or nothing.
    fn apply(&self, changes: &[Change]) -> Result<(), StorageError>;

    /// Replace the whole state (checkpoint / migration repair). Also one
    /// transaction.
    fn replace_all(&self, state: &PersistedState) -> Result<(), StorageError>;
}
