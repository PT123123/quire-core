// Persistence — SQLite repository + migrations (M3).
// Implements `core::persistence::Repository`; nothing here may know about
// Slint (docs/ARCHITECTURE.md hard rule 2).

pub mod backup;
pub mod data_location;
pub mod database;
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
