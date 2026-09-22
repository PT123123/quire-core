// Persistence — SQLite repository + migrations (M3).
// Implements `core::persistence::Repository`; nothing here may know about
// Slint (docs/ARCHITECTURE.md hard rule 2).

pub mod backup;
pub mod backlinks;
pub mod data_location;
pub mod database;
/// The view rules' SQL compiler (SPEC §三十九 「操作」, Track 3 D4 / ADR-0076):
/// the filter tree, the sort list and the group of one view become this
/// statement's `WHERE` / `ORDER BY` / group predicate — text and binds, and
/// nothing else. `database_store` is the only caller that executes what this
/// builds, which is what makes 「filter / sort 在 SQL 侧完成，不在 UI 侧过滤」
/// a module boundary rather than a rule someone has to remember.
pub mod database_query;
/// The database layer's SQL (SPEC §三十九): `databases`, `db_properties`,
/// `db_records`, `db_values`, `db_value_items`, `db_views`, and the windowed
/// row read. A separate module from `database` (which owns the `Connection`),
/// so this slice adds a file instead of editing one.
pub mod database_store;
pub mod migrations;
pub mod repository;
pub mod search_index;

pub use backup::OpenReport;
pub use database::Database;
pub use repository::SqliteRepository;
