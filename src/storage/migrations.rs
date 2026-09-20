// Schema versions, applied in order and tracked via `PRAGMA user_version`.
// Versioned migrations (SPEC §二十五 "safe migration"): a v1 database may
// only ever be upgraded forward, never edited in place.

use rusqlite::{Connection, OptionalExtension};

use crate::core::StorageError;

/// The schema version this build of Quire expects.
pub const CURRENT_VERSION: i32 = 7;

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
CREATE VIRTUAL TABLE IF NOT EXISTS search_pages USING fts5(
    title,
    tokenize = 'unicode61'
);

CREATE VIRTUAL TABLE IF NOT EXISTS search_blocks USING fts5(
    page_id UNINDEXED,
    text,
    tokenize = 'unicode61'
);
"#,
    backfill: Some(super::search_index::rebuild),
},
Migration {
    version: 3,
    label: "inline marks (M6)",
    // One row per styled range; ranges are byte offsets into the block text.
    // Non-overlapping per (block, kind) is an app-layer invariant.
    sql: r#"
CREATE TABLE IF NOT EXISTS marks (
    block INTEGER NOT NULL REFERENCES blocks(id) ON DELETE CASCADE,
    start INTEGER NOT NULL,
    end   INTEGER NOT NULL,
    kind  TEXT NOT NULL,
    url   TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (block, start, kind)
);

CREATE INDEX IF NOT EXISTS idx_marks_block ON marks(block);
"#,
    backfill: None,
}, Migration {
    version: 4,
    label: "block colors",
    // Block-level color pair (ADR-0023): '' = theme default. Two flat
    // columns instead of a color table — the palette is closed and the
    // values are opaque strings to SQL. SQLite has no `ADD COLUMN IF NOT
    // EXISTS`, so the columns are added conditionally in code: a file that
    // was hand-downgraded (the migration test does exactly that) or
    // restored from a newer snapshot may already carry them.
    sql: "",
    backfill: Some(add_color_columns),
}, Migration {
    version: 5,
    label: "page-block reference",
    // `blocks.page_ref` — the page a Page-kind block opens (NULL = none).
    // Nullable INTEGER, added conditionally like the color columns: a
    // hand-downgraded or snapshot-restored file may already carry it.
    sql: "",
    backfill: Some(add_page_ref_column),
}, Migration {
    version: 6,
    label: "block fold state",
    // `blocks.folded` — the block's subtree is hidden in the editor
    // (SPEC §三十七). Added conditionally like the other late columns.
    sql: "",
    backfill: Some(add_folded_column),
}, Migration {
    version: 7,
    label: "attachments",
    // SPEC §三十七 批次 A: the media layer. `attachments` holds one row per
    // file that lives in the folder next to this database — the row is the
    // reference, the bytes are on disk. `blocks.attachment` points at it and
    // carries no FK: a block whose file row is gone must still load, and
    // render as a missing picture, rather than fail the whole library.
    sql: r#"
CREATE TABLE IF NOT EXISTS attachments (
    id     INTEGER PRIMARY KEY,
    name   TEXT NOT NULL,
    file   TEXT NOT NULL,
    thumb  TEXT NOT NULL DEFAULT '',
    mime   TEXT NOT NULL DEFAULT '',
    bytes  INTEGER NOT NULL DEFAULT 0,
    width  INTEGER NOT NULL DEFAULT 0,
    height INTEGER NOT NULL DEFAULT 0
);
"#,
    backfill: Some(add_attachment_columns),
}];

/// Migration 4 body: add each color column only when it is missing.
fn add_color_columns(conn: &mut Connection) -> Result<(), StorageError> {
    const COLUMNS: [(&str, &str); 2] = [
        ("color", "ALTER TABLE blocks ADD COLUMN color TEXT NOT NULL DEFAULT ''"),
        ("bg", "ALTER TABLE blocks ADD COLUMN bg TEXT NOT NULL DEFAULT ''"),
    ];
    for (name, ddl) in COLUMNS {
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        if present == 0 {
            conn.execute(ddl, [])
                .map_err(|e| StorageError::Sql(format!("add {name}: {e}")))?;
        }
    }
    Ok(())
}

/// Migration 5 body: add `blocks.page_ref` only when it is missing.
fn add_page_ref_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'page_ref'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute("ALTER TABLE blocks ADD COLUMN page_ref INTEGER", [])
            .map_err(|e| StorageError::Sql(format!("add page_ref: {e}")))?;
    }
    Ok(())
}

/// Migration 6 body: add `blocks.folded` only when it is missing.
fn add_folded_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'folded'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute(
            "ALTER TABLE blocks ADD COLUMN folded INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| StorageError::Sql(format!("add folded: {e}")))?;
    }
    Ok(())
}

/// Migration 7 body: the two block-side attachment columns, each added only
/// when it is missing (same shape as the color pair above).
fn add_attachment_columns(conn: &mut Connection) -> Result<(), StorageError> {
    const COLUMNS: [(&str, &str); 2] = [
        ("attachment", "ALTER TABLE blocks ADD COLUMN attachment INTEGER"),
        (
            "img_percent",
            "ALTER TABLE blocks ADD COLUMN img_percent INTEGER NOT NULL DEFAULT 100",
        ),
    ];
    for (name, ddl) in COLUMNS {
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        if present == 0 {
            conn.execute(ddl, [])
                .map_err(|e| StorageError::Sql(format!("add {name}: {e}")))?;
        }
    }
    Ok(())
}

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
    const TABLES: [&str; 9] = [
        "workspaces",
        "pages",
        "blocks",
        "block_children",
        "attachments",
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
