// Rust Core — data structures for documents (M3+).
// See docs/ARCHITECTURE.md; nothing here may know about Slint types.

pub mod command;
pub mod database;
// D6's computing half of SPEC §三十九 「需计算」: the formula engine — a pure
// lexer and hand-written interpreter, finite by constants (ADR-0082/0083).
pub mod database_formula;
pub mod database_property;
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

