// Persistence — SQLite repository + migrations (M3).
// Implements `core::persistence::Repository`; nothing here may know about
// Slint (docs/ARCHITECTURE.md hard rule 2).

pub mod backup;
pub mod data_location;
pub mod database;
pub mod migrations;
pub mod repository;
pub mod search_index;

pub use backup::OpenReport;
pub use database::Database;
pub use repository::SqliteRepository;
