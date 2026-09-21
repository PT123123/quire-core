// Persistence contract: what the M3 storage layer must provide to the rest
// of the app. Core defines it; `src/storage/` implements it (SQLite, M3).
//
// Model: the app holds the full in-memory state and emits ordered change
// lists (ADR-0012). `apply` is the only write path and must be one
// transaction per call — an intermediate state may never be observable
// (SPEC §十八). `load` and `replace_all` are the bulk paths (startup and
// checkpoint/repair).

use std::fmt;

use super::types::{
    Attachment, AttachmentId, Block, BlockId, BlockKind, ColorKind, Lang, Mark, OrderKey, Page,
    PageFont, PageId, PersistedState,
};

/// Ordered list of persisted mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    PageCreated(Page),
    PageTitleSet { id: PageId, title: String },
    PageMoved { id: PageId, parent: Option<PageId>, order: OrderKey },
    PageFavoriteSet { id: PageId, favorite: bool },
    PageExpandedSet { id: PageId, expanded: bool },
    /// The typeface one page's document tier uses (SPEC §三十八). Page-level on
    /// purpose: the blocks store no font, so this cannot be expressed as a
    /// block change even though it is read while drawing them.
    PageFontSet { id: PageId, font: PageFont },
    /// The two page layout switches (SPEC §三十八). They travel together
    /// because they share one column, and a switch that does not move simply
    /// keeps its bit.
    PageLayoutSet {
        id: PageId,
        full_width: bool,
        small_text: bool,
    },
    /// The page's own icon (SPEC §三十八 "图标与封面"): the emoji, or `""` for
    /// none. Stored rather than indexed so the picker's list can change without
    /// touching anybody's page. Like the font above it, this is a page column
    /// and cannot be expressed against a block.
    PageIconSet { id: PageId, icon: String },
    /// Storage deletes the page and (recursively) its sub-pages.
    PageDeleted { id: PageId },

    BlockInserted(Block),
    BlockTextSet { id: BlockId, text: String },
    BlockKindSet { id: BlockId, kind: BlockKind },
    BlockCheckedSet { id: BlockId, checked: bool },
    /// Fold state (SPEC §三十七): the block's subtree is hidden in the editor
    /// while set. Persisted view state, like `PageExpandedSet`.
    BlockFoldedSet { id: BlockId, folded: bool },
    /// Replace the block's whole inline-mark list (M6).
    BlockMarksSet { id: BlockId, marks: Vec<Mark> },
    BlockMoved { id: BlockId, parent: Option<BlockId>, order: OrderKey },
    /// Cross-page move of one block (the whole subtree moves as a list of
    /// these, one per block; `parent`/`order` keep every child attached to
    /// its moved parent, so the subtree arrives intact).
    BlockMovedToPage { id: BlockId, page: PageId, parent: Option<BlockId>, order: OrderKey },
    /// Block-level color pair (text + row background). Both travel together
    /// so one change covers a "Text: Red" or "Background: Blue" pick.
    BlockColorSet { id: BlockId, color: ColorKind, background: ColorKind },
    /// Point a `Page`-kind block at a page (`None` clears it). The page
    /// itself is created/destroyed by the surrounding `PageCreated` /
    /// `PageDeleted` changes in the same batch, not here.
    BlockRefSet { id: BlockId, page: Option<PageId> },
    /// Point an `Image` block at an attachment (SPEC §三十七 批次 A). The
    /// file row itself arrives in the same batch as `AttachmentAdded`; undo
    /// clears this pointer and leaves the file alone (see `AttachmentAdded`).
    BlockAttachmentSet {
        id: BlockId,
        attachment: Option<AttachmentId>,
    },
    /// Display width of an `Image` block, in percent of the editor column.
    BlockImageWidthSet { id: BlockId, percent: u16 },
    /// Column count of a `Table` block (SPEC §三十七 批次 B). `0` = not a
    /// table; adding/removing a column is one of these plus the cell inserts
    /// or deletes it implies.
    BlockColumnsSet { id: BlockId, columns: u16 },
    /// The language a `Code` block is coloured as (SPEC §三十七 批次 C). Colour
    /// only: a block this build cannot lex stores `Plain`, so an imported fence
    /// never fails and never loses a character either.
    BlockLangSet { id: BlockId, lang: Lang },
    /// Upsert one attachment row. The bytes are already on disk by the time
    /// this is recorded, so undo removes the *reference* only and never the
    /// file: an orphaned picture is recoverable, a deleted one is not.
    AttachmentAdded(Attachment),
    /// Drop one attachment row for good (SPEC §三十七, ADR-0037). This is the
    /// one change the *reclaim* emits and no command plan does: undo must never
    /// reach it, or a Ctrl+Z would restore a reference to bytes that are gone.
    /// Deleting an absent row is a no-op, like `MetaDelete`.
    AttachmentDeleted { id: AttachmentId },
    /// Storage deletes the block and (recursively) its children; undo
    /// replays the captured subtree as `BlockInserted`s.
    BlockDeleted { id: BlockId },

    MetaSet { key: String, value: String },
    /// Storage removes the metadata row. Deleting an absent key is a no-op,
    /// so a consumer can drain a key without a prior read (M8_FEEDBACK #1).
    MetaDelete { key: String },
    SettingSet { key: String, value: String },
    /// Storage removes the settings row — the real replacement for
    /// settings_store's empty-value tombstone (M8_FEEDBACK #1).
    SettingDelete { key: String },
}

/// Every attachment id a change list points at, read off the arm that carries
/// it. Two callers need the same answer and must not disagree (§三十七,
/// ADR-0037): the undo stack, whose outstanding entries can put a reference
/// back into the document, and the reclaim, which may only delete what neither
/// the document nor that stack still names. `BlockDeleted` is deliberately not
/// one of them — the delete is what *drops* a reference, and its undo carries
/// the `BlockInserted` that holds the id.
pub fn attachment_ids_in(changes: &[Change]) -> impl Iterator<Item = AttachmentId> + '_ {
    changes.iter().filter_map(|change| match change {
        Change::BlockInserted(block) => block.attachment,
        Change::BlockAttachmentSet { attachment, .. } => *attachment,
        Change::AttachmentAdded(attachment) => Some(attachment.id),
        _ => None,
    })
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
