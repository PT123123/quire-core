// Rust Core — data structures for documents (M3+).
// See docs/ARCHITECTURE.md; nothing here may know about Slint types.

pub mod command;
pub mod database;
// D6's computing half of SPEC §三十九 「需计算」: the formula engine — a pure
// lexer and hand-written interpreter, finite by constants (ADR-0082/0083).
pub mod database_formula;
pub mod database_property;
// SPEC §三十九 「需计算」's last two thirds, which ADR-0084 handed over and
// ADR-0088/0089 built: a relation is a stored list of target record ids with an
// involution for its back-pointer, and a rollup folds one column of the records
// that relation names (computed at projection time, stored nowhere).
pub mod database_relation;
pub mod database_rollup;
// D7's 数据库模板: the record prefill document — a copy of content in the
// store's own shapes, never a second content format (ADR-0086).
pub mod database_template;
// D3's drawing half of SPEC §三十九: the view's definition document, the
// columns it shows, and the shape one window of rows takes on its way to Slint.
pub mod database_view;
pub mod date;
pub mod diff;
pub mod document;
pub mod embed;
pub mod highlight;
pub mod history;
pub mod icon;
pub mod math;
pub mod persistence;
pub mod reference;
pub mod template;
pub mod types;

pub use command::{exec, plan, redo, undo, Command};
pub use date::{is_iso_date, today_iso};
pub use document::Document;
pub use history::History;
pub use persistence::{Change, Repository, StorageError};
pub use reference::{page_of, page_uri, trigger_at, PAGE_SCHEME};
pub use types::{
    Attachment, AttachmentId, Block, BlockId, BlockKind, ColorKind, Lang, Mark, MarkKind, OrderKey,
    Page, PageFont, PageId, PersistedState,
};

