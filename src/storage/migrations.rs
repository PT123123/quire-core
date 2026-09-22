// Schema versions, applied in order and tracked via `PRAGMA user_version`.
// Versioned migrations (SPEC §二十五 "safe migration"): a v1 database may
// only ever be upgraded forward, never edited in place.

use rusqlite::{Connection, OptionalExtension};

use crate::core::StorageError;

/// The schema version this build of Quire expects. Steps 12–15, 17, 18 and 20
/// are Track 3's (the database layer, ADR-0060–0087; 20 is the record
/// template, ADR-0086); step 16 is Track 2's and step 19 is Track 4's, both
/// written in other trees — `ensure_current` applies every step above the
/// file's version in array order, so the gap closes when the branches land
/// and a fresh file ends at 20.
pub const CURRENT_VERSION: i32 = 20;

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
}, Migration {
    version: 8,
    label: "table columns",
    // SPEC §三十七 批次 B: `blocks.columns` is how many boxes a container
    // holds — the column count of a Table block, or the side-by-side box count
    // of a Columns layout (0 = neither). Both containers' contents are ordinary
    // child blocks, so nothing else needs storing and the columns slice needed
    // no new migration. Added conditionally like every other late column.
    sql: "",
    backfill: Some(add_columns_column),
}, Migration {
    version: 9,
    label: "code language",
    // SPEC §三十七 批次 C: `blocks.lang` is the language a code block is
    // coloured as, and the only thing the colour keys on — the characters stay
    // in `text`, and every coloured layer is derived from them at paint time.
    // `''` = no colour on this block, which is also what a code block is
    // before anyone picks one, so the default is the whole migration.
    // Added conditionally like every other late column.
    sql: "",
    backfill: Some(add_lang_column),
}, Migration {
    version: 10,
    label: "page appearance",
    // SPEC §三十八: a page's own look, which §十七's tree never stored.
    // `pages.font` is the document tier's typeface ('' = the default stack)
    // and `pages.layout` is a bit field -- 1 = full width, 2 = small text.
    // Both are page-level and neither is per-block: the block text keeps
    // storing characters, and the size that draws them is derived at paint
    // time from the page. Added conditionally like every other late column.
    sql: "",
    backfill: Some(add_page_appearance_columns),
}, Migration {
    version: 11,
    label: "page icon",
    // SPEC §三十八 "图标与封面": `pages.icon` holds the emoji a page shows in
    // its sidebar slot and above its own title. The emoji is stored, not an
    // index into the picker's list, so the list can grow without rewriting a
    // page. `''` = unset, and an unset page falls back to the first character
    // of its title at draw time -- so the default is the whole migration and a
    // v10 library opens with every page exactly as it looked before.
    sql: "",
    backfill: Some(add_page_icon_column),
}, Migration {
    version: 12,
    label: "databases",
    // SPEC §三十九 (ADR-0060): the database entity itself. One row per
    // database, reached from a page through a `Database` block's `db_ref` (a
    // later step, with the block kind). Deliberately only this table: the
    // schema, the rows and the views are three more semantic units below, and a
    // migration is the one thing in this project that cannot be undone —
    // one step per unit is what keeps a half-applied upgrade readable.
    sql: r#"
CREATE TABLE IF NOT EXISTS databases (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL DEFAULT ''
);
"#,
    backfill: None,
}, Migration {
    version: 13,
    label: "database properties",
    // ADR-0061: a database's columns are rows, because `UNIQUE (db, name)` is
    // what makes renaming a column well defined — an invariant no JSON blob can
    // enforce. Only a select's option list is JSON, and it lives in `config`.
    // `ord` is an app invariant (like `block_children.ord`), not a constraint.
    sql: r#"
CREATE TABLE IF NOT EXISTS db_properties (
    id     INTEGER PRIMARY KEY,
    db     INTEGER NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    name   TEXT NOT NULL,
    kind   TEXT NOT NULL,
    config TEXT NOT NULL DEFAULT '',
    ord    INTEGER NOT NULL,
    UNIQUE (db, name)
);

CREATE INDEX IF NOT EXISTS idx_db_properties_db ON db_properties(db);
"#,
    backfill: None,
}, Migration {
    version: 14,
    label: "database records and values",
    // ADR-0063's records and ADR-0062's values, in one step because they are
    // one unit of meaning: a value without its record is not a state any
    // version of this app can produce, and `db_records.page`'s CASCADE is the
    // backstop the record/page contract is written against. `UNIQUE (page)` is
    // the ownership: a page is the face of at most one record, and two rows can
    // never share one (SQLite allows many NULLs under it, which is load-bearing
    // — a bare record is the default, ADR-0063).
    //
    // `db_values` is one row per (record, property) with one column per SQLite
    // type a comparison needs: `text`, `num`, `flag`. A comparison has to
    // happen in SQLite's own type system, or `ORDER BY` is lexicographic and
    // `10` sorts before `9` (ADR-0062). `db_value_items` carries the list
    // kinds — multi-select option ids and files attachment ids — so "has this
    // option" is an index probe and not a JSON scan.
    sql: r#"
CREATE TABLE IF NOT EXISTS db_records (
    id   INTEGER PRIMARY KEY,
    db   INTEGER NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    page INTEGER REFERENCES pages(id) ON DELETE CASCADE,
    ord  INTEGER NOT NULL,
    UNIQUE (page)
);

CREATE INDEX IF NOT EXISTS idx_db_records_db_ord ON db_records(db, ord);

CREATE TABLE IF NOT EXISTS db_values (
    record   INTEGER NOT NULL REFERENCES db_records(id) ON DELETE CASCADE,
    property INTEGER NOT NULL REFERENCES db_properties(id) ON DELETE CASCADE,
    text     TEXT NOT NULL DEFAULT '',
    num      REAL,
    flag     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (record, property)
);

CREATE INDEX IF NOT EXISTS idx_db_values_property ON db_values(property);

CREATE TABLE IF NOT EXISTS db_value_items (
    record   INTEGER NOT NULL REFERENCES db_records(id) ON DELETE CASCADE,
    property INTEGER NOT NULL REFERENCES db_properties(id) ON DELETE CASCADE,
    ord      INTEGER NOT NULL,
    value    TEXT NOT NULL,
    PRIMARY KEY (record, property, ord)
);
"#,
    backfill: None,
}, Migration {
    version: 15,
    label: "database views",
    // ADR-0064: a view is a row whose name, layout and place in the switcher
    // are columns, and whose rules (filter / sorts / groups / visible columns /
    // widths) are one JSON document in `definition`. The split is ADR-0061's
    // and ADR-0062's test read the other way round: SQL has to *list* views and
    // their names, so those are columns; SQL never filters *on* the rules, so
    // their shape is whatever the compiler reads best — and a filter is a tree
    // whose depth nothing bounds.
    sql: r#"
CREATE TABLE IF NOT EXISTS db_views (
    id         INTEGER PRIMARY KEY,
    db         INTEGER NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    layout     TEXT NOT NULL DEFAULT 'table',
    definition TEXT NOT NULL DEFAULT '',
    ord        INTEGER NOT NULL,
    UNIQUE (db, name)
);

CREATE INDEX IF NOT EXISTS idx_db_views_db ON db_views(db);
"#,
    backfill: None,
}, Migration {
    version: 16,
    label: "reference lookup",
    // SPEC §四十 / ADR-0051: the backlink panel asks "which blocks point at
    // this page", and it must not be answered by a full scan of `marks` or of
    // `blocks` — the panel is drawn with every page open, so the cost is on the
    // interactive path.
    //
    // **Two indexes and no table.** A reference is already stored exactly once:
    // a mention keeps `quire://page/<id>` in the one payload column `marks`
    // has (ADR-0050), and a block-level reference keeps `blocks.page_ref`
    // (ADR-0026). Nothing new is written, so nothing new can drift, and there
    // is no maintenance path for a bug to hide in — SQLite keeps a B-tree in
    // step with the rows it indexes, which is the only kind of derived data
    // this project has never had to send a sweep after.
    //
    // `(kind, url)` is what makes the mention probe an index seek: the pair is
    // exactly the predicate, and the leftmost column alone (kind) is *not*
    // enough — every mark of the same kind would still be scanned, and a page
    // with 10 000 bold spans has no mentions to find. `page_ref` gets its own
    // index for the same reason.
    sql: r#"
CREATE INDEX IF NOT EXISTS idx_marks_reference ON marks(kind, url);
CREATE INDEX IF NOT EXISTS idx_blocks_page_ref ON blocks(page_ref);
"#,
    // An index is built by SQLite from rows that are already there, so a v15
    // library needs no backfill step: the panel reads the same data it would
    // have read, only faster.
    backfill: None,
}, Migration {
    version: 17,
    label: "database record timestamps",
    // ADR-0068: `created time` and `last edited time` are §三十九's two derived
    // kinds, and they are derived from **these two columns and nothing else**.
    // Writing them into `db_values` would be the double write ADR-0039 forbids
    // — an instant kept in the same table as the cells whose changing it is
    // supposed to notice — so the record's own row carries them and the
    // projection reads them from there (a cell of either kind is never stored,
    // and a write aimed at one is refused by name).
    //
    // The shape is ADR-0062's stored date: `YYYY-MM-DDTHH:MM`, local wall time,
    // fixed width, `''` meaning "not known" (which is every row a v16 file
    // already had — an upgrade invents no birthdays). Text rather than a Unix
    // integer on purpose: both date-shaped kinds then sort by one rule, because
    // fixed-width bytes are chronological bytes, and neither the reader nor the
    // writer needs a calendar in Rust — SQLite's `strftime` is the clock
    // (`storage::database_store`).
    //
    // A step of its own rather than a line added to v14: migrations are the one
    // thing in this project that cannot be undone, and a v14 file in the wild
    // (or in a backup) must keep meaning what it meant.
    sql: "",
    backfill: Some(add_record_timestamp_columns),
}, Migration {
    version: 18,
    label: "database block reference",
    // SPEC §三十九 / ADR-0060, the second half of the decision v12 opened: a
    // `Database` block points at its entity through `blocks.db_ref`, the shape
    // `blocks.page_ref` has had since migration 5 and for the same reason —
    // the block *draws* the entity without *being* it, because a record may
    // itself be a page and §三十九's "a record may be a page" has to keep a
    // page's identity (`pages.id` is what §四十's mentions point at).
    //
    // Nullable and with no `DEFAULT`, like `page_ref`: `NULL` is "this block is
    // not a database block", which is every row a v17 file already has. No
    // foreign key either, and the same argument ADR-0026 made for `page_ref`:
    // the entity is deleted by the change that drops the block (ADR-0060), and
    // a block whose ref dangles is a *state* the renderer has a word for
    // ("(deleted database)") — a constraint that refused the write would turn a
    // recoverable picture into a failed transaction.
    //
    // No index: nothing asks "which blocks draw this database" yet. D7's linked
    // database is the query that will want one, and adding an index for a
    // question nobody asks is a B-tree every write pays for.
    sql: "",
    backfill: Some(add_db_ref_column),
}, Migration {
    version: 19,
    label: "block sync pointer",
    // SPEC §四十 / ADR-0052: a `Synced` block is a second view of one other
    // block, so it needs one place to keep that address — and only that. Its
    // `text` stays empty for good, which is the part worth holding onto: the
    // moment a mirror also carried the words, there would be two owners of one
    // sentence and nothing that could ever prove which one won.
    //
    // **Nullable, no `DEFAULT`, and above all no foreign key.** `NULL` reads as
    // "no source", which covers both a mirror whose source has not been picked
    // and one whose source was deleted — the projection cannot tell those two
    // apart and must not have to; ADR-0052 §2 makes both read as the same
    // read-only placeholder. A `REFERENCES blocks(id)` here would drag
    // `ON DELETE CASCADE` semantics along with it, and cascading is precisely
    // what this decision refuses: deleting a *view* must never take the content
    // with it.
    //
    // No index either, for the reason v18 gives: nothing yet asks "which blocks
    // mirror this one". If such a panel ever exists it gets its own step, and
    // an index built for a question nobody asks is a B-tree every write pays
    // for.
    //
    // **Why 19.** 17 and 18 are Track 3's database steps and one of them is
    // already committed. Migrations are the only irreversible thing in this
    // project, so two features sharing a number costs more than a number that
    // is briefly unused in one isolated tree — that gap closes the moment the
    // trees meet, and whose file proves it is the integrator's arithmetic, not
    // mine.
    sql: "",
    backfill: Some(add_sync_ref_column),
}, Migration {
    version: 20,
    label: "database record template",
    // SPEC §三十九 「操作」's 数据库模板 (Track 3 D7, ADR-0086): the prefill a
    // database's new records start from, as one JSON document on the
    // `databases` row itself. A column and not a table, for the reason
    // ADR-0064 gave for view documents: SQL never filters on a template —
    // nothing asks "which databases prefill this value" — so a second table
    // would be a join nobody runs, and the document is the database's own
    // truth, replaced whole by `Change::DatabaseTemplateSet` (the same
    // whole-document discipline `db_properties.config` and `db_views.definition`
    // follow). The values inside it are the shapes `db_values` already stores
    // (ADR-0086), so applying a template is the ordinary cell write and no
    // second content format exists to keep in step.
    //
    // `NOT NULL DEFAULT ''` keeps `''` meaning "no template" — the same
    // "empty string is the absence" convention `pages.icon` (v11) uses, and
    // `database_template::cells_of("")` folds to "no prefill", so a pre-v20
    // library's databases read as exactly what they were: untemplated.
    //
    // **Why 20.** 12–15, 17 and 18 are Track 3's earlier steps; 16 is Track
    // 2's and 19 is Track 4's, both written after 18 in other trees. The
    // runner applies every step above the file's version in array order, so a
    // gap in one isolated tree closes the moment the trees meet — v17's
    // landing proved that shape.
    sql: "",
    backfill: Some(add_database_template_column),
}];

/// Add each named column to `pages`, only when that column is missing. Every
/// late `pages` column goes through this, so a partially-migrated file (a
/// column an older build added by hand, a backup restored mid-step) converges
/// instead of erroring on a duplicate name.
fn add_page_columns(conn: &mut Connection, columns: &[(&str, &str)]) -> Result<(), StorageError> {
    for (name, ddl) in columns {
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('pages') WHERE name = ?1",
                [*name],
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

/// Migration 11 body.
fn add_page_icon_column(conn: &mut Connection) -> Result<(), StorageError> {
    add_page_columns(
        conn,
        &[("icon", "ALTER TABLE pages ADD COLUMN icon TEXT NOT NULL DEFAULT ''")],
    )
}

/// Migration 17 body: the two record timestamps, each added only when it is
/// missing (ADR-0068). Same shape as the color and attachment pairs above: a
/// half-applied upgrade converges instead of erroring on a duplicate name, and
/// a file the columns were added to by hand still opens.
fn add_record_timestamp_columns(conn: &mut Connection) -> Result<(), StorageError> {
    const COLUMNS: [(&str, &str); 2] = [
        (
            "created",
            "ALTER TABLE db_records ADD COLUMN created TEXT NOT NULL DEFAULT ''",
        ),
        (
            "edited",
            "ALTER TABLE db_records ADD COLUMN edited TEXT NOT NULL DEFAULT ''",
        ),
    ];
    for (name, ddl) in COLUMNS {
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('db_records') WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .map_err(|e| StorageError::Sql(e.to_string()))?;
        if present == 0 {
            conn.execute(ddl, [])
                .map_err(|e| StorageError::Sql(format!("add db_records.{name}: {e}")))?;
        }
    }
    Ok(())
}

/// Migration 20 body: `databases.template`, added only when it is missing —
/// the same "缺哪列补哪列" convergence the record timestamps (v17) use, so a
/// half-applied upgrade or a hand-edited file still opens.
fn add_database_template_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('databases') WHERE name = 'template'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute(
            "ALTER TABLE databases ADD COLUMN template TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| StorageError::Sql(format!("add databases.template: {e}")))?;
    }
    Ok(())
}

/// Migration 10 body: the two page-appearance columns.
fn add_page_appearance_columns(conn: &mut Connection) -> Result<(), StorageError> {
    add_page_columns(
        conn,
        &[
            ("font", "ALTER TABLE pages ADD COLUMN font TEXT NOT NULL DEFAULT ''"),
            (
                "layout",
                "ALTER TABLE pages ADD COLUMN layout INTEGER NOT NULL DEFAULT 0",
            ),
        ],
    )
}

/// Migration 9 body: add `blocks.lang` only when it is missing.
fn add_lang_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'lang'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute(
            "ALTER TABLE blocks ADD COLUMN lang TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| StorageError::Sql(format!("add lang: {e}")))?;
    }
    Ok(())
}

/// Migration 8 body: add `blocks.columns` only when it is missing.
fn add_columns_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'columns'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute(
            "ALTER TABLE blocks ADD COLUMN columns INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| StorageError::Sql(format!("add columns: {e}")))?;
    }
    Ok(())
}

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

/// Migration 18 body: add `blocks.db_ref` only when it is missing (ADR-0060).
/// The same "缺哪列补哪列" shape as `page_ref`, `folded` and the two attachment
/// columns: a half-applied upgrade — the column added by hand, a backup restored
/// mid-step — converges instead of erroring on a duplicate name.
fn add_db_ref_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'db_ref'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute("ALTER TABLE blocks ADD COLUMN db_ref INTEGER", [])
            .map_err(|e| StorageError::Sql(format!("add db_ref: {e}")))?;
    }
    Ok(())
}

/// Migration 19 body: add `blocks.sync_ref` only when it is missing — same
/// defensive shape as every late `blocks` column, so a file some older build
/// (or a hand) half-upgraded converges instead of failing on a duplicate name.
fn add_sync_ref_column(conn: &mut Connection) -> Result<(), StorageError> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'sync_ref'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Sql(e.to_string()))?;
    if present == 0 {
        conn.execute("ALTER TABLE blocks ADD COLUMN sync_ref INTEGER", [])
            .map_err(|e| StorageError::Sql(format!("add sync_ref: {e}")))?;
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
    const TABLES: [&str; 15] = [
        "workspaces",
        "pages",
        "blocks",
        "block_children",
        "attachments",
        "metadata",
        "settings",
        "search_pages",
        "search_blocks",
        // SPEC §三十九 (v12–v15). A file that reaches here without them is a
        // file whose upgrade did not run, which is what this check is for.
        "databases",
        "db_properties",
        "db_records",
        "db_values",
        "db_value_items",
        "db_views",
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
