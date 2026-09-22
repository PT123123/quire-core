// Persistence contract: what the M3 storage layer must provide to the rest
// of the app. Core defines it; `src/storage/` implements it (SQLite, M3).
//
// Model: the app holds the full in-memory state and emits ordered change
// lists (ADR-0012). `apply` is the only write path and must be one
// transaction per call — an intermediate state may never be observable
// (SPEC §十八). `load` and `replace_all` are the bulk paths (startup and
// checkpoint/repair).

use std::fmt;

use super::database::{
    CellValue, Database, DatabaseId, Property, PropertyId, PropertyKind, Record, RecordId, View,
    ViewId, ViewLayout,
};
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

    // ─── SPEC §三十九 Database (Track 3, D1) ─────────────────────────────────
    //
    // The database layer's write path, appended at the end of the enum like
    // every variant before it: nothing here renumbers, and an id is an id.
    // One `apply` is one transaction (SPEC §十八), which is what makes a record
    // and its page one undo step: the command layer plans `apply` and `revert`
    // as change lists (`core::document::Entry`), and storage never has to know
    // which direction it is running in.
    /// A new database entity (ADR-0060). ADR-0061 says a database that cannot
    /// be drawn is one no path may create, so the caller writes this together
    /// with its `title` property and its first view (`Database::title_property`
    /// / `first_view`) in the same batch.
    DatabaseCreated(Database),
    DatabaseRenamed { id: DatabaseId, name: String },
    /// Storage deletes the entity; its properties, views, records, values and
    /// list items all cascade (ADR-0061/0062/0063/0064). The pages its records
    /// own do **not**: a page is owned by a record, never by the database
    /// (ADR-0063), so a page the user made survives the database that showed it.
    DatabaseDeleted { id: DatabaseId },

    PropertyAdded(Property),
    PropertyRenamed { id: PropertyId, name: String },
    /// The column's type. The values already stored are left exactly where they
    /// are — a kind change is not a conversion, and casting a column's values
    /// is D2's, with its own rules per type pair. Nothing double-writes.
    PropertyKindSet { id: PropertyId, kind: PropertyKind },
    PropertyOrdSet { id: PropertyId, ord: OrderKey },
    /// Storage deletes the column and every value stored in it (`ON DELETE
    /// CASCADE`, ADR-0062). View documents that name the id are JSON, which no
    /// foreign key can reach: ADR-0064's compiler drops an unknown id instead.
    PropertyDeleted { id: PropertyId },

    /// A new row. `page` is `None` for a bare record — ADR-0063's lazy page:
    /// creating a row creates no page, and a page arrives when someone opens
    /// the row.
    RecordCreated(Record),
    RecordOrdSet { id: RecordId, ord: OrderKey },
    /// Point the record at a page, or clear the pointer. Opening a record is
    /// this plus the `PageCreated` and the title move in the same batch; its
    /// inverse ("Turn into a plain record") moves the title back and leaves the
    /// page in the tree — that operation is about the pointer (ADR-0063).
    RecordPageSet { id: RecordId, page: Option<PageId> },
    /// Storage deletes the row and its values. A page the record owned is *not*
    /// deleted here: ADR-0063's delete plans `[DbValueDeleted…, RecordDeleted,
    /// PageDeleted?]`, and keeping the two separate is what lets one `revert`
    /// put back exactly what was there.
    RecordDeleted { id: RecordId },
    /// One cell, in ADR-0062's stored shape. `Empty` removes the row (and any
    /// items) rather than writing a blank: absence is the one representation of
    /// empty, so a number cell is never `0` and a text cell the user cleared is
    /// `Text("")`, which is a row.
    CellSet {
        record: RecordId,
        property: PropertyId,
        value: CellValue,
    },

    ViewAdded(View),
    ViewRenamed { id: ViewId, name: String },
    ViewLayoutSet { id: ViewId, layout: ViewLayout },
    /// The view's rules — filter, sorts, groups, visible columns, widths — as
    /// one JSON document, replaced whole (ADR-0064). Replaced rather than
    /// merged: the document is the view's own truth, and merging it in storage
    /// would put the compiler's shape in two places.
    ViewDefinitionSet { id: ViewId, definition: String },
    ViewOrdSet { id: ViewId, ord: OrderKey },
    ViewDeleted { id: ViewId },

    /// Point a `Database` block at the entity it draws, or clear the pointer
    /// (SPEC §三十九, ADR-0060). Appended after D1's block rather than beside
    /// `BlockRefSet` because the enum is append-only: a variant's position is
    /// nothing, and its spelling is everything.
    ///
    /// This is the write `Command::MakeDatabase` emits, and it is deliberately
    /// the *only* one that touches the column: a `Database` block whose entity
    /// is gone is a state the read paths survive (ADR-0060's "(deleted
    /// database)"), while a block whose ref was silently repointed would be a
    /// database someone else's rows vanished into.
    BlockDbRefSet { id: BlockId, db: Option<DatabaseId> },

    /// A column's `config` document, **replaced whole** (ADR-0061's one JSON
    /// document per column; the read-edit-write discipline is ADR-0074's,
    /// applied to a column instead of a view). D6's first writer is the formula
    /// expression (ADR-0082: the config holds the *expression* — the value is
    /// computed at projection time and is never stored, ADR-0062/0039), and a
    /// later editor of an option list or a number format writes through the
    /// same arm rather than through a variant of its own: the document is the
    /// column's own truth, and merging it in storage would put the writer's
    /// shape in two places.
    ///
    /// Like `ViewDefinitionSet`, there is no `from` here — the command layer
    /// (`Command::SetDatabaseFormula`) captured the previous document and its
    /// revert names it; a change names what happened, not which way it ran.
    PropertyConfigSet { id: PropertyId, config: String },
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
