// Schema versions, applied in order and tracked via `PRAGMA user_version`.
// Versioned migrations (SPEC §二十五 "safe migration"): a v1 database may
// only ever be upgraded forward, never edited in place.

use rusqlite::{Connection, OptionalExtension};

use crate::core::StorageError;

/// The schema version this build of Quire expects.
pub const CURRENT_VERSION: i32 = 2;

/// A single forward-only schema step: `sql` runs when the database sits at
/// `version - 1` and bumps `user_version` to `version`. `backfill`, when
/// present, runs right after the step commits (it needs its own transaction).
struct Migration {
    version: i32,
    label: &'static str,
    sql: &'static str,
    backfill: Option<fn(&mut Connection) -> Result<(), StorageError>>,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    label: "initial",
    backfill: None,
    sql: r#"
CREATE TABLE workspaces (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE pages (
    id       INTEGER PRIMARY KEY,
    title    TEXT NOT NULL DEFAULT '',
    parent   INTEGER REFERENCES pages(id) ON DELETE CASCADE,
    ord      INTEGER NOT NULL,
    favorite INTEGER NOT NULL DEFAULT 0,
    expanded INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE blocks (
    id      INTEGER PRIMARY KEY,
    page    INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    kind    TEXT NOT NULL,
    text    TEXT NOT NULL DEFAULT '',
    checked INTEGER NOT NULL DEFAULT 0
);

-- Tree position of a block, split out per SPEC §十二: `parent` NULL means
-- top level of the page. One row per block, inserted with the block.
CREATE TABLE block_children (
    block  INTEGER PRIMARY KEY REFERENCES blocks(id) ON DELETE CASCADE,
    parent INTEGER REFERENCES blocks(id) ON DELETE CASCADE,
    ord    INTEGER NOT NULL
);

CREATE TABLE metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX idx_pages_parent     ON pages(parent);
CREATE INDEX idx_blocks_page      ON blocks(page);
CREATE INDEX idx_block_children_p ON block_children(parent);

INSERT INTO workspaces (id, name) VALUES (1, 'Workspace');
"#,
}, Migration {
    version: 2,
    label: "search-index",
    // Full-text search (SPEC §二十, ADR-0014). `unicode61` alone cannot match
    // Chinese: the index stores a segmented copy of the text (see
    // storage/search_index.rs), so the tokenizer needs no custom table here.
    sql: r#"
CREATE VIRTUAL TABLE search_pages USING fts5(
    title,
    tokenize = 'unicode61'
);

CREATE VIRTUAL TABLE search_blocks USING fts5(
    page_id UNINDEXED,
    text,
    tokenize = 'unicode61'
);
"#,
    backfill: Some(super::search_index::rebuild),
}];

pub fn user_version(conn: &Connection) -> Result<i32, StorageError> {
    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
        .map_err(|e| StorageError::Open(e.to_string()))
}

/// Bring `conn` to `CURRENT_VERSION`. A fresh (version 0) database gets the
/// whole chain; an already-current one is a no-op. A version above ours
/// means the binary is older than the data — refuse rather than downgrade.
pub fn ensure_current(conn: &mut Connection) -> Result<(), StorageError> {
    let from = user_version(conn)?;
    if from > CURRENT_VERSION {
        return Err(StorageError::Corrupt(format!(
            "database schema version {from} is newer than this build \
             (max {CURRENT_VERSION}); refusing to open"
        )));
    }
    for migration in MIGRATIONS {
        if migration.version <= from {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        tx.execute_batch(migration.sql)
            .map_err(|e| StorageError::Sql(format!("migration {} failed: {e}", migration.label)))?;
        // user_version is a compile-time literal here, not user data
        tx.pragma_update(None, "user_version", migration.version)
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        tx.commit()
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        if let Some(backfill) = migration.backfill {
            backfill(conn)
                .map_err(|e| StorageError::Sql(format!("migration {} backfill: {e}", migration.label)))?;
        }
    }
    Ok(())
}

/// True when every table the current schema needs is present.
pub fn check_schema(conn: &Connection) -> Result<(), StorageError> {
    const TABLES: [&str; 8] = [
        "workspaces",
        "pages",
        "blocks",
        "block_children",
        "metadata",
        "settings",
        "search_pages",
        "search_blocks",
    ];
    for table in TABLES {
        let found: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        if found.is_none() {
            return Err(StorageError::Corrupt(format!(
                "missing table {table} at schema version {}",
                user_version(conn)?
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_reaches_current_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_current(&mut conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_VERSION);
        // idempotent
        ensure_current(&mut conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), CURRENT_VERSION);
        check_schema(&conn).unwrap();
    }

    #[test]
    fn newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", CURRENT_VERSION + 1)
            .unwrap();
        assert!(matches!(
            ensure_current(&mut conn),
            Err(StorageError::Corrupt(_))
        ));
    }

    #[test]
    fn schema_check_catches_missing_table() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            check_schema(&conn),
            Err(StorageError::Corrupt(_))
        ));
    }
}
