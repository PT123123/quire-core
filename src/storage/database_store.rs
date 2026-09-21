// The database layer's SQL (SPEC §三十九, ADR-0060…ADR-0067): reads and writes
// for `databases`, `db_properties`, `db_records`, `db_values`,
// `db_value_items` and `db_views`.
//
// Two things here are the point of the whole track, and both are shapes rather
// than discipline:
//
//   1. **A row exists only inside a window.** `window_rows` takes the window
//      D0's projection computed (`core::database::window`) and runs it as the
//      query's `LIMIT`/`OFFSET`, so a 10 000-row database hands back 31 rows and
//      the other 9 969 never become objects. Nothing in the app reads rows any
//      other way: `unwindowed_rows` exists as the measurement's control arm and
//      for the Markdown export ADR-0065 describes, and both callers know they
//      are asking for the whole table (ADR-0067).
//   2. **The title has one home** (ADR-0063). A record that owns a page reads
//      its title from `pages.title`; a bare record reads it from the value row
//      of its `title` property. One `COALESCE` covers both, in the same query
//      that fetches the window, so there is no second copy to go stale.
//
// The write path is one function per `Change` arm, called from
// `repository::apply_one` (which is where the exhaustive match lives, so a new
// variant cannot be forgotten). Everything runs inside the caller's transaction
// and nothing commits on its own (SPEC §十八).

use std::collections::{BTreeSet, HashMap};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::core::database::{
    CellValue, Database, DatabaseCatalog, DatabaseId, Property, PropertyId, PropertyKind, RealizedRows,
    Record, RecordId, RowRequest, RowView, RowWindow, ValueKind, View, ViewGeometry, ViewId,
};
use crate::core::persistence::StorageError;
use crate::core::types::{OrderKey, PageId};

use super::database::{ord_from_db, ord_to_db};
use super::repository::{require_hit, SqliteRepository};

fn sql(e: rusqlite::Error) -> StorageError {
    StorageError::Sql(e.to_string())
}

/// Every id this module binds is a `u64` on the way in and an `i64` in SQLite.
fn id(value: u64) -> i64 {
    value as i64
}

/// A count as an id-free `usize`; a negative count cannot happen, but a
/// `usize` cast of `-1` is a 18-quintillion-row database, so it is clamped.
fn count(value: i64) -> usize {
    value.max(0) as usize
}

// ─── reads ──────────────────────────────────────────────────────────────────

impl SqliteRepository {
    /// The schema half of the database layer — every database, its properties
    /// and its views — and deliberately **no rows**: ADR-0067 makes a window
    /// the only way records reach memory, so what a startup load carries is the
    /// part whose size is a schema's (a handful of columns, a few views) and not
    /// a table's.
    pub fn load_databases(&self) -> Result<DatabaseCatalog, StorageError> {
        let conn = self.database().conn();
        let mut catalog = DatabaseCatalog::default();
        {
            let mut stmt = conn
                .prepare("SELECT id, name FROM databases ORDER BY id")
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Database {
                        id: DatabaseId(r.get::<_, i64>(0)? as u64),
                        name: r.get(1)?,
                    })
                })
                .map_err(sql)?;
            for row in rows {
                catalog.databases.push(row.map_err(sql)?);
            }
        }
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, db, name, kind, config, ord
                     FROM db_properties ORDER BY db, ord, id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(sql)?;
            for row in rows {
                let (id, db, name, kind, config, ord) = row.map_err(sql)?;
                catalog.properties.push(Property {
                    id: PropertyId(id as u64),
                    db: DatabaseId(db as u64),
                    name,
                    // ADR-0061's fold: a kind this build does not know is text,
                    // never a failed open.
                    kind: PropertyKind::from_stored(&kind),
                    config,
                    ord: OrderKey(ord_from_db(ord)),
                });
            }
        }
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, db, name, layout, definition, ord
                     FROM db_views ORDER BY db, ord, id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(sql)?;
            for row in rows {
                let (id, db, name, layout, definition, ord) = row.map_err(sql)?;
                catalog.views.push(View {
                    id: ViewId(id as u64),
                    db: DatabaseId(db as u64),
                    name,
                    // An unknown layout is a table (ADR-0064), so a view written
                    // by a later build opens in the shape this one knows.
                    layout: crate::core::database::ViewLayout::from_stored(&layout),
                    definition,
                    ord: OrderKey(ord_from_db(ord)),
                });
            }
        }
        Ok(catalog)
    }

    /// One record's own row: its owner, its page and its place in the listing.
    /// `None` means there is no such record — a row the view drew and someone
    /// deleted is a row the next read does not find.
    pub fn record(&self, id: RecordId) -> Result<Option<Record>, StorageError> {
        let conn = self.database().conn();
        conn.query_row(
            "SELECT db, page, ord FROM db_records WHERE id = ?1",
            params![id.as_u64() as i64],
            |r| {
                Ok(Record {
                    id,
                    db: DatabaseId(r.get::<_, i64>(0)? as u64),
                    page: r.get::<_, Option<i64>>(1)?.map(|p| PageId(p as u64)),
                    ord: OrderKey(ord_from_db(r.get::<_, i64>(2)?)),
                })
            },
        )
        .optional()
        .map_err(sql)
    }

    /// How many rows the database has — the number the window is computed from,
    /// and the only thing a 10 000-row database costs before anyone scrolls.
    /// (D4's filters narrow this; the count and the window stay the same pair.)
    pub fn record_count(&self, db: DatabaseId) -> Result<usize, StorageError> {
        let conn = self.database().conn();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM db_records WHERE db = ?1",
                params![db.as_u64() as i64],
                |r| r.get(0),
            )
            .map_err(sql)?;
        Ok(count(n))
    }

    /// A record's title, wherever its one home is (ADR-0063): the page's title
    /// when the record owns a page, the `title` property's value row when it
    /// does not. `None` means no such record; an existing record with an empty
    /// title is `Some("")`.
    pub fn record_title(&self, id: RecordId) -> Result<Option<String>, StorageError> {
        let conn = self.database().conn();
        let title: Option<Option<String>> = conn
            .query_row(
                "SELECT COALESCE(p.title, v.text)
                 FROM db_records r
                 LEFT JOIN pages p ON p.id = r.page
                 LEFT JOIN db_values v ON v.record = r.id
                      AND v.property = (SELECT id FROM db_properties
                                        WHERE db = r.db AND kind = 'title'
                                        ORDER BY ord, id LIMIT 1)
                 WHERE r.id = ?1",
                params![id.as_u64() as i64],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sql)?;
        Ok(title.map(|t| t.unwrap_or_default()))
    }

    /// One cell, in the shape its column stores (ADR-0062): the `text` column,
    /// the `num` column, the `flag` column, or `db_value_items`. A property
    /// that does not exist, a column that stores nothing (`formula` / `rollup`
    /// / `relation`), and a cell nobody ever wrote all answer `Empty` — which is
    /// this design's single representation of "no value".
    pub fn cell(&self, record: RecordId, property: PropertyId) -> Result<CellValue, StorageError> {
        let conn = self.database().conn();
        let kind: Option<String> = conn
            .query_row(
                "SELECT kind FROM db_properties WHERE id = ?1",
                params![property.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        let Some(kind) = kind else {
            return Ok(CellValue::Empty);
        };
        match PropertyKind::from_stored(&kind).value_kind() {
            ValueKind::Computed => Ok(CellValue::Empty),
            ValueKind::Items => {
                let items = read_cell_items(&conn, record, property)?;
                if items.is_empty() {
                    Ok(CellValue::Empty)
                } else {
                    Ok(CellValue::Items(items))
                }
            }
            shape => {
                let row: Option<(String, Option<f64>, i64)> = conn
                    .query_row(
                        "SELECT text, num, flag FROM db_values
                         WHERE record = ?1 AND property = ?2",
                        params![record.as_u64() as i64, property.as_u64() as i64],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(sql)?;
                Ok(match (row, shape) {
                    // No row at all is `Empty` whatever the kind…
                    (None, _) => CellValue::Empty,
                    // …and so is a number column whose `num` is NULL: ADR-0062
                    // makes "empty" and "not a number" the same thing, never 0.
                    (Some((_, None, _)), ValueKind::Number) => CellValue::Empty,
                    (Some((text, _, _)), ValueKind::Text) => CellValue::Text(text),
                    (Some((_, Some(num), _)), ValueKind::Number) => CellValue::Number(num),
                    (Some((_, _, flag)), ValueKind::Flag) => CellValue::Flag(flag != 0),
                    (Some(_), _) => CellValue::Empty,
                })
            }
        }
    }

    /// The rows of one window, and only those: the query ends in the window's
    /// own `LIMIT`/`OFFSET`, so the number of rows that come back *is* the
    /// window's length (ADR-0067). This is what D0's `RealizedRows::scroll_to`
    /// passes as its `fetch`.
    pub fn window_rows(
        &self,
        req: &RowRequest<'_>,
        window: RowWindow,
    ) -> Result<Vec<RowView>, StorageError> {
        read_rows(&self.database().conn(), req, Some(window))
    }

    /// The whole table in one `Vec` — **the control arm of the measurement, not
    /// a reader.** A view may not call this (ADR-0067); it exists so a window's
    /// cost can be compared against the table's at the same moment, and so the
    /// Markdown export (ADR-0065, which writes a file and has no viewport) has
    /// a name to argue about — it should argue for a streaming read instead.
    pub fn unwindowed_rows(&self, req: &RowRequest<'_>) -> Result<Vec<RowView>, StorageError> {
        read_rows(&self.database().conn(), req, None)
    }

    /// The projection and the query, wired together: `COUNT(*)` for the total,
    /// `core::database::window` for the window, and exactly that window's rows
    /// from SQL. The arcs are D0's and D1's halves meeting — the window is
    /// still computed *before* any row is asked for, and the fetch is handed
    /// nothing but the window.
    pub fn realized_rows(
        &self,
        req: &RowRequest<'_>,
        geometry: ViewGeometry,
        scroll_y: f32,
    ) -> Result<RealizedRows, StorageError> {
        let total = self.record_count(req.db)?;
        let mut failure = None;
        let realized = RealizedRows::scroll_to(total, geometry, scroll_y, |window| {
            match self.window_rows(req, window) {
                Ok(rows) => rows,
                // A failed read must not become an empty window the caller
                // believes in: the error is carried out beside the model.
                Err(e) => {
                    failure = Some(e);
                    Vec::new()
                }
            }
        });
        match failure {
            Some(e) => Err(e),
            None => Ok(realized),
        }
    }
}

/// The one query a row read runs.
///
/// One `LEFT JOIN` per visible property against `db_values` on
/// `(record, property)` — the primary key, so every join is an index probe
/// rather than a scan — plus the record's page. The row's title is ADR-0063's
/// `COALESCE(p.title, t.text)`, and a *visible* title column reads through the
/// same expression: a page-backed record's title is `pages.title` and nothing
/// else, whether the view shows the column or not.
///
/// `LIMIT`/`OFFSET` are the window. They are appended only when a window was
/// asked for, so the control read is the same query minus those two clauses and
/// the difference between the two numbers is the window and nothing else.
fn row_query(req: &RowRequest<'_>, window: Option<RowWindow>) -> String {
    let mut select = String::from("SELECT r.id, COALESCE(p.title, t.text)");
    let mut from = String::from(
        " FROM db_records r \
         LEFT JOIN pages p ON p.id = r.page \
         LEFT JOIN db_values t ON t.record = r.id AND t.property = ?1",
    );
    for (index, column) in req.columns.iter().enumerate() {
        let alias = format!("v{index}");
        let text = if column.id == req.title {
            format!("COALESCE(p.title, {alias}.text)")
        } else {
            format!("{alias}.text")
        };
        select.push_str(&format!(", {text}, {alias}.num, {alias}.flag"));
        from.push_str(&format!(
            " LEFT JOIN db_values {alias} ON {alias}.record = r.id AND {alias}.property = ?{}",
            index + 2
        ));
    }
    let mut query = format!("{select}{from} WHERE r.db = ?{} ORDER BY r.ord, r.id", req.columns.len() + 2);
    if window.is_some() {
        query.push_str(&format!(
            " LIMIT ?{} OFFSET ?{}",
            req.columns.len() + 3,
            req.columns.len() + 4
        ));
    }
    query
}

/// The bind values `row_query`'s placeholders take, in order: the title
/// property, the visible properties, the database, then the window.
fn row_binds(req: &RowRequest<'_>, window: Option<RowWindow>) -> Vec<i64> {
    let mut binds = Vec::with_capacity(req.columns.len() + 4);
    binds.push(id(req.title.as_u64()));
    binds.extend(req.columns.iter().map(|c| id(c.id.as_u64())));
    binds.push(id(req.db.as_u64()));
    if let Some(window) = window {
        binds.push(window.len() as i64);
        binds.push(window.start as i64);
    }
    binds
}

/// Run `row_query` and assemble the rows. Cells come back in `columns` order
/// and are painted through `CellValue::display` — D1's placeholder rendering,
/// which D2 replaces with the per-type one.
fn read_rows(
    conn: &Connection,
    req: &RowRequest<'_>,
    window: Option<RowWindow>,
) -> Result<Vec<RowView>, StorageError> {
    let query = row_query(req, window);
    let binds = row_binds(req, window);
    let mut stmt = conn.prepare(&query).map_err(sql)?;
    let mut raw: Vec<RawRow> = Vec::new();
    {
        let mut rows = stmt
            .query(params_from_iter(binds.iter().copied()))
            .map_err(sql)?;
        while let Some(row) = rows.next().map_err(sql)? {
            let record = row.get::<_, i64>(0).map_err(sql)?;
            let title = row.get::<_, Option<String>>(1).map_err(sql)?;
            let mut cells = Vec::with_capacity(req.columns.len());
            for index in 0..req.columns.len() {
                let at = 2 + index * 3;
                cells.push((
                    row.get::<_, Option<String>>(at).map_err(sql)?,
                    row.get::<_, Option<f64>>(at + 1).map_err(sql)?,
                    row.get::<_, Option<i64>>(at + 2).map_err(sql)?,
                ));
            }
            raw.push((record, title, cells));
        }
    }

    // The list kinds live in `db_value_items`, one row per item, so a record
    // with three options is three rows — reading them as extra columns would
    // multiply the window's rows by the longest list in it. One more query for
    // the window's records and only the list-typed columns, in `ord` order so
    // the chosen order is the display order.
    let list_columns: Vec<i64> = req
        .columns
        .iter()
        .filter(|c| c.kind.value_kind() == ValueKind::Items)
        .map(|c| id(c.id.as_u64()))
        .collect();
    let items = if list_columns.is_empty() || raw.is_empty() {
        HashMap::new()
    } else {
        let records: Vec<i64> = raw.iter().map(|(record, _, _)| *record).collect();
        read_items(conn, &records, &list_columns)?
    };

    let mut out = Vec::with_capacity(raw.len());
    for (record, title, cells) in raw {
        let mut painted = Vec::with_capacity(req.columns.len());
        for (column, (text, num, flag)) in req.columns.iter().zip(cells) {
            // A list column's value came from its own query, keyed by column:
            // there is no `db_values` row behind it (ADR-0062).
            let value = match column.kind.value_kind() {
                ValueKind::Items => match items.get(&(record, id(column.id.as_u64()))) {
                    Some(list) if !list.is_empty() => CellValue::Items(list.clone()),
                    _ => CellValue::Empty,
                },
                _ => cell_from_columns(column.kind, text, num, flag),
            };
            painted.push(value.display());
        }
        out.push(RowView {
            record: record as u64,
            title: title.unwrap_or_default(),
            cells: painted,
        });
    }
    Ok(out)
}

/// One row as it came out of SQL: the record id, the title column of the
/// `COALESCE`, and the (text, num, flag) triple per visible property.
type RawRow = (i64, Option<String>, Vec<(Option<String>, Option<f64>, Option<i64>)>);

/// Which of ADR-0062's storage shapes a value came back in. The `text` column
/// is `NOT NULL DEFAULT ''`, so a NULL in it means the `LEFT JOIN` found no row
/// at all — which is what tells "empty" from "a text cell the user blanked".
fn cell_from_columns(
    kind: PropertyKind,
    text: Option<String>,
    num: Option<f64>,
    flag: Option<i64>,
) -> CellValue {
    match kind.value_kind() {
        // Computed columns have no value row by construction; a list column's
        // items are folded in by the caller.
        ValueKind::Computed | ValueKind::Items => CellValue::Empty,
        ValueKind::Number => num.map(CellValue::Number).unwrap_or(CellValue::Empty),
        ValueKind::Flag => flag
            .map(|f| CellValue::Flag(f != 0))
            .unwrap_or(CellValue::Empty),
        ValueKind::Text => text.map(CellValue::Text).unwrap_or(CellValue::Empty),
    }
}

/// The items of every (record, property) among `records` and `properties`.
fn read_items(
    conn: &Connection,
    records: &[i64],
    properties: &[i64],
) -> Result<HashMap<(i64, i64), Vec<String>>, StorageError> {
    let record_list = placeholders(records.len());
    let property_list = placeholders(properties.len());
    let query = format!(
        "SELECT record, property, value FROM db_value_items
         WHERE record IN ({record_list}) AND property IN ({property_list})
         ORDER BY record, property, ord"
    );
    let mut binds: Vec<i64> = Vec::with_capacity(records.len() + properties.len());
    binds.extend_from_slice(records);
    binds.extend_from_slice(properties);
    let mut stmt = conn.prepare(&query).map_err(sql)?;
    let mut rows = stmt
        .query(params_from_iter(binds.iter().copied()))
        .map_err(sql)?;
    let mut out: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    while let Some(row) = rows.next().map_err(sql)? {
        let record = row.get::<_, i64>(0).map_err(sql)?;
        let property = row.get::<_, i64>(1).map_err(sql)?;
        let value = row.get::<_, String>(2).map_err(sql)?;
        out.entry((record, property)).or_default().push(value);
    }
    Ok(out)
}

/// One cell's items, in their stored order.
fn read_cell_items(
    conn: &Connection,
    record: RecordId,
    property: PropertyId,
) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn
        .prepare(
            "SELECT value FROM db_value_items
             WHERE record = ?1 AND property = ?2 ORDER BY ord",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map(
            params![record.as_u64() as i64, property.as_u64() as i64],
            |r| r.get::<_, String>(0),
        )
        .map_err(sql)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql)
}

/// `?, ?, ?` for an `IN` list of `n` values. The window bounds the list on
/// purpose: a query built from a whole table's ids would be the unwindowed
/// read wearing a different hat.
fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n.max(1)).collect::<Vec<_>>().join(", ")
}

// ─── writes ─────────────────────────────────────────────────────────────────
//
// One function per `Change` arm, all inside the caller's transaction. Names are
// the change's name so `repository::apply_one` reads as a table of contents.

pub(crate) fn insert_database(tx: &Transaction, db: &Database) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO databases (id, name) VALUES (?1, ?2)",
        params![id(db.id.as_u64()), db.name],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn rename_database(
    tx: &Transaction,
    db: DatabaseId,
    name: &str,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE databases SET name = ?2 WHERE id = ?1",
            params![id(db.as_u64()), name],
        )
        .map_err(sql)?;
    require_hit(n, "DatabaseRenamed", db.as_u64())
}

pub(crate) fn delete_database(tx: &Transaction, db: DatabaseId) -> Result<(), StorageError> {
    // The schema, the rows, their values and their views go with it through the
    // foreign keys (ADR-0061/0062/0063/0064). The pages its records owned do
    // not: a page belongs to a record (ADR-0063), so it is deleted by whoever
    // deletes the record, never as a side effect of the database.
    let n = tx
        .execute(
            "DELETE FROM databases WHERE id = ?1",
            params![id(db.as_u64())],
        )
        .map_err(sql)?;
    require_hit(n, "DatabaseDeleted", db.as_u64())
}

pub(crate) fn insert_property(tx: &Transaction, property: &Property) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO db_properties (id, db, name, kind, config, ord)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id(property.id.as_u64()),
            id(property.db.as_u64()),
            property.name,
            property.kind.as_str(),
            property.config,
            ord_to_db(property.ord.0),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn rename_property(
    tx: &Transaction,
    property: PropertyId,
    name: &str,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_properties SET name = ?2 WHERE id = ?1",
            params![id(property.as_u64()), name],
        )
        .map_err(sql)?;
    require_hit(n, "PropertyRenamed", property.as_u64())
}

pub(crate) fn set_property_kind(
    tx: &Transaction,
    property: PropertyId,
    kind: PropertyKind,
) -> Result<(), StorageError> {
    // The values are left where they are. A kind change is not a conversion:
    // converting a column's values is D2's, per type pair, and doing it here
    // would be a write path with no undo of its own.
    let n = tx
        .execute(
            "UPDATE db_properties SET kind = ?2 WHERE id = ?1",
            params![id(property.as_u64()), kind.as_str()],
        )
        .map_err(sql)?;
    require_hit(n, "PropertyKindSet", property.as_u64())
}

pub(crate) fn set_property_ord(
    tx: &Transaction,
    property: PropertyId,
    ord: OrderKey,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_properties SET ord = ?2 WHERE id = ?1",
            params![id(property.as_u64()), ord_to_db(ord.0)],
        )
        .map_err(sql)?;
    require_hit(n, "PropertyOrdSet", property.as_u64())
}

pub(crate) fn delete_property(tx: &Transaction, property: PropertyId) -> Result<(), StorageError> {
    // Values and items follow through the FKs (ADR-0062). A view's JSON
    // document that names this id is unreachable from SQL and is not rewritten
    // here: ADR-0064's compiler drops an unknown id, so the view shows more rows
    // rather than failing to open.
    let n = tx
        .execute(
            "DELETE FROM db_properties WHERE id = ?1",
            params![id(property.as_u64())],
        )
        .map_err(sql)?;
    require_hit(n, "PropertyDeleted", property.as_u64())
}

pub(crate) fn insert_record(tx: &Transaction, record: &Record) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO db_records (id, db, page, ord) VALUES (?1, ?2, ?3, ?4)",
        params![
            id(record.id.as_u64()),
            id(record.db.as_u64()),
            record.page.map(|p| id(p.as_u64())),
            ord_to_db(record.ord.0),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn set_record_ord(
    tx: &Transaction,
    record: RecordId,
    ord: OrderKey,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_records SET ord = ?2 WHERE id = ?1",
            params![id(record.as_u64()), ord_to_db(ord.0)],
        )
        .map_err(sql)?;
    require_hit(n, "RecordOrdSet", record.as_u64())
}

pub(crate) fn set_record_page(
    tx: &Transaction,
    record: RecordId,
    page: Option<PageId>,
) -> Result<(), StorageError> {
    // `UNIQUE (page)` is the ownership (ADR-0063): this is the statement that
    // refuses a second record for a page the database already shows.
    let n = tx
        .execute(
            "UPDATE db_records SET page = ?2 WHERE id = ?1",
            params![id(record.as_u64()), page.map(|p| id(p.as_u64()))],
        )
        .map_err(sql)?;
    require_hit(n, "RecordPageSet", record.as_u64())
}

pub(crate) fn delete_record(tx: &Transaction, record: RecordId) -> Result<(), StorageError> {
    // The values and the list items follow through the FKs (ADR-0062). A page
    // the record owned is deleted by the `PageDeleted` the command puts in the
    // same batch (ADR-0063) — this statement deliberately leaves it alone, so
    // that one `revert` can put back exactly what was there.
    let n = tx
        .execute(
            "DELETE FROM db_records WHERE id = ?1",
            params![id(record.as_u64())],
        )
        .map_err(sql)?;
    require_hit(n, "RecordDeleted", record.as_u64())
}

pub(crate) fn set_cell(
    tx: &Transaction,
    record: RecordId,
    property: PropertyId,
    value: &CellValue,
) -> Result<(), StorageError> {
    let (record, property) = (id(record.as_u64()), id(property.as_u64()));
    // One cell is one row (ADR-0062), so every shape is an upsert of the same
    // primary key. The *shape* decides which column the value lands in, which
    // is why a caller writes the shape its property's kind stores; validating
    // that here would put a `SELECT` on the cell-write path D6 has to measure.
    let clear_items = "DELETE FROM db_value_items WHERE record = ?1 AND property = ?2";
    let upsert = "INSERT INTO db_values (record, property, text, num, flag)
                  VALUES (?1, ?2, ?3, ?4, ?5)
                  ON CONFLICT(record, property) DO UPDATE SET
                      text = excluded.text, num = excluded.num, flag = excluded.flag";
    match value {
        // Empty is the absence of the row, never a blank value. An empty list
        // is normalised to this too: a list cell with nothing in it is the same
        // absence as every other (ADR-0062).
        CellValue::Empty => {
            tx.execute("DELETE FROM db_values WHERE record = ?1 AND property = ?2", params![record, property])
                .map_err(sql)?;
            tx.execute(clear_items, params![record, property]).map_err(sql)?;
        }
        CellValue::Items(items) if items.is_empty() => {
            tx.execute("DELETE FROM db_values WHERE record = ?1 AND property = ?2", params![record, property])
                .map_err(sql)?;
            tx.execute(clear_items, params![record, property]).map_err(sql)?;
        }
        // A list lives in `db_value_items` and nowhere else: writing an empty
        // `db_values` row for it would make an empty list read back as a blank
        // text cell.
        CellValue::Items(items) => {
            tx.execute("DELETE FROM db_values WHERE record = ?1 AND property = ?2", params![record, property])
                .map_err(sql)?;
            tx.execute(clear_items, params![record, property]).map_err(sql)?;
            for (ord, item) in items.iter().enumerate() {
                tx.execute(
                    "INSERT INTO db_value_items (record, property, ord, value)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![record, property, ord as i64, item],
                )
                .map_err(sql)?;
            }
        }
        CellValue::Text(text) => {
            tx.execute(upsert, params![record, property, text, None::<f64>, 0i64])
                .map_err(sql)?;
            tx.execute(clear_items, params![record, property]).map_err(sql)?;
        }
        CellValue::Number(num) => {
            tx.execute(upsert, params![record, property, "", Some(*num), 0i64])
                .map_err(sql)?;
            tx.execute(clear_items, params![record, property]).map_err(sql)?;
        }
        CellValue::Flag(flag) => {
            tx.execute(upsert, params![record, property, "", None::<f64>, i64::from(*flag)])
                .map_err(sql)?;
            tx.execute(clear_items, params![record, property]).map_err(sql)?;
        }
    }
    Ok(())
}

pub(crate) fn insert_view(tx: &Transaction, view: &View) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO db_views (id, db, name, layout, definition, ord)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id(view.id.as_u64()),
            id(view.db.as_u64()),
            view.name,
            view.layout.as_str(),
            view.definition,
            ord_to_db(view.ord.0),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn rename_view(
    tx: &Transaction,
    view: ViewId,
    name: &str,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_views SET name = ?2 WHERE id = ?1",
            params![id(view.as_u64()), name],
        )
        .map_err(sql)?;
    require_hit(n, "ViewRenamed", view.as_u64())
}

pub(crate) fn set_view_layout(
    tx: &Transaction,
    view: ViewId,
    layout: crate::core::database::ViewLayout,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_views SET layout = ?2 WHERE id = ?1",
            params![id(view.as_u64()), layout.as_str()],
        )
        .map_err(sql)?;
    require_hit(n, "ViewLayoutSet", view.as_u64())
}

pub(crate) fn set_view_definition(
    tx: &Transaction,
    view: ViewId,
    definition: &str,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_views SET definition = ?2 WHERE id = ?1",
            params![id(view.as_u64()), definition],
        )
        .map_err(sql)?;
    require_hit(n, "ViewDefinitionSet", view.as_u64())
}

pub(crate) fn set_view_ord(
    tx: &Transaction,
    view: ViewId,
    ord: OrderKey,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_views SET ord = ?2 WHERE id = ?1",
            params![id(view.as_u64()), ord_to_db(ord.0)],
        )
        .map_err(sql)?;
    require_hit(n, "ViewOrdSet", view.as_u64())
}

pub(crate) fn delete_view(tx: &Transaction, view: ViewId) -> Result<(), StorageError> {
    let n = tx
        .execute("DELETE FROM db_views WHERE id = ?1", params![id(view.as_u64())])
        .map_err(sql)?;
    require_hit(n, "ViewDeleted", view.as_u64())
}

// ─── the bulk path's snapshot ───────────────────────────────────────────────
//
// `replace_all` (checkpoint / repair / a LAN pull) replaces the *document*:
// pages, blocks, meta, settings. It is not handed the database layer, and
// `DELETE FROM pages` cascades through `db_records.page` (ADR-0063) — so the
// database layer has to be picked up before that statement and put back after
// it, which is what these two functions do. ADR-0066 is where the rule is
// written down: the layer survives, except where it hangs off a page the
// incoming state dropped, and then the record goes with its page exactly as it
// does on the ordinary delete path.

/// Raw rows of the six tables, exactly as they were.
pub(crate) struct DatabaseSnapshot {
    databases: Vec<(i64, String)>,
    properties: Vec<(i64, i64, String, String, String, i64)>,
    records: Vec<(i64, i64, Option<i64>, i64)>,
    values: Vec<(i64, i64, String, Option<f64>, i64)>,
    items: Vec<(i64, i64, i64, String)>,
    views: Vec<(i64, i64, String, String, String, i64)>,
}

pub(crate) fn snapshot_tables(conn: &Connection) -> Result<DatabaseSnapshot, StorageError> {
    let mut snapshot = DatabaseSnapshot {
        databases: Vec::new(),
        properties: Vec::new(),
        records: Vec::new(),
        values: Vec::new(),
        items: Vec::new(),
        views: Vec::new(),
    };
    {
        let mut stmt = conn.prepare("SELECT id, name FROM databases").map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?;
        for row in rows {
            snapshot.databases.push(row.map_err(sql)?);
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT id, db, name, kind, config, ord FROM db_properties")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .map_err(sql)?;
        for row in rows {
            snapshot.properties.push(row.map_err(sql)?);
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT id, db, page, ord FROM db_records")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(sql)?;
        for row in rows {
            snapshot.records.push(row.map_err(sql)?);
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT record, property, text, num, flag FROM db_values")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(sql)?;
        for row in rows {
            snapshot.values.push(row.map_err(sql)?);
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT record, property, ord, value FROM db_value_items")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(sql)?;
        for row in rows {
            snapshot.items.push(row.map_err(sql)?);
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT id, db, name, layout, definition, ord FROM db_views")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .map_err(sql)?;
        for row in rows {
            snapshot.views.push(row.map_err(sql)?);
        }
    }
    Ok(snapshot)
}

/// Put a snapshot back, skipping the records whose page the incoming state no
/// longer has — and with them every value that hangs off those records, because
/// a record whose page is gone is gone (ADR-0063's invariant, ADR-0066's rule
/// for this path). Bare records always survive: they own nothing.
pub(crate) fn restore_tables(
    tx: &Transaction,
    snapshot: &DatabaseSnapshot,
    kept_pages: &BTreeSet<i64>,
) -> Result<(), StorageError> {
    for (id, name) in &snapshot.databases {
        tx.execute(
            "INSERT OR REPLACE INTO databases (id, name) VALUES (?1, ?2)",
            params![id, name],
        )
        .map_err(sql)?;
    }
    for (id, db, name, kind, config, ord) in &snapshot.properties {
        tx.execute(
            "INSERT OR REPLACE INTO db_properties (id, db, name, kind, config, ord)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, db, name, kind, config, ord],
        )
        .map_err(sql)?;
    }
    let mut kept: BTreeSet<i64> = BTreeSet::new();
    for (id, db, page, ord) in &snapshot.records {
        if page.is_some_and(|p| !kept_pages.contains(&p)) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO db_records (id, db, page, ord) VALUES (?1, ?2, ?3, ?4)",
            params![id, db, page, ord],
        )
        .map_err(sql)?;
        kept.insert(*id);
    }
    for (record, property, text, num, flag) in &snapshot.values {
        if !kept.contains(record) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO db_values (record, property, text, num, flag)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![record, property, text, num, flag],
        )
        .map_err(sql)?;
    }
    for (record, property, ord, value) in &snapshot.items {
        if !kept.contains(record) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO db_value_items (record, property, ord, value)
             VALUES (?1, ?2, ?3, ?4)",
            params![record, property, ord, value],
        )
        .map_err(sql)?;
    }
    for (id, db, name, layout, definition, ord) in &snapshot.views {
        tx.execute(
            "INSERT OR REPLACE INTO db_views (id, db, name, layout, definition, ord)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, db, name, layout, definition, ord],
        )
        .map_err(sql)?;
    }
    Ok(())
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::database::{ViewLayout, TITLE_PROPERTY_NAME};
    use crate::core::persistence::{Change, Repository};
    use crate::core::types::{Page, PageFont};

    fn store() -> SqliteRepository {
        SqliteRepository::in_memory().expect("in-memory repository")
    }

    fn property(id: u64, kind: PropertyKind, ordinal: u64) -> Property {
        Property {
            id: PropertyId(id),
            db: DatabaseId(1),
            name: format!("P{id}"),
            kind,
            config: String::new(),
            ord: OrderKey(OrderKey::FIRST.0 + ordinal * 0x100),
        }
    }

    fn page(id: u64, title: &str) -> Page {
        Page {
            id: PageId(id),
            title: title.into(),
            parent: None,
            order: OrderKey::FIRST,
            favorite: false,
            expanded: false,
            font: PageFont::default(),
            full_width: false,
            small_text: false,
            icon: String::new(),
        }
    }

    /// A database with its title column, its first view and whatever extra
    /// columns the test needs — the three rows ADR-0061 says arrive together.
    fn seed(repo: &SqliteRepository, extra: &[Property]) -> Database {
        let db = Database::new(DatabaseId(1), "Tasks");
        let mut changes = vec![
            Change::DatabaseCreated(db.clone()),
            Change::PropertyAdded(db.title_property(PropertyId(1))),
            Change::ViewAdded(db.first_view(ViewId(1))),
        ];
        changes.extend(extra.iter().cloned().map(Change::PropertyAdded));
        repo.apply(&changes).unwrap();
        db
    }

    /// `count` bare records in a database that is already seeded, each with a
    /// title in the title column, and their ids in listing order.
    fn rows(repo: &SqliteRepository, count: usize) -> Vec<RecordId> {
        let mut changes = Vec::with_capacity(count * 2);
        for index in 0..count {
            let record = RecordId(1_000 + index as u64);
            changes.push(Change::RecordCreated(Record::bare(
                record,
                DatabaseId(1),
                OrderKey(((index as u64) + 1) << 32),
            )));
            changes.push(Change::CellSet {
                record,
                property: PropertyId(1),
                value: CellValue::Text(format!("Row {index}")),
            });
        }
        repo.apply(&changes).unwrap();
        (0..count).map(|i| RecordId(1_000 + i as u64)).collect()
    }

    /// How many rows one table holds, for the tests that are about what SQL
    /// really stored rather than about what a read answers.
    fn raw_count(repo: &SqliteRepository, table: &str) -> i64 {
        let conn = repo.database().conn();
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_databases_schema_round_trips_and_a_foreign_kind_folds_to_text() {
        let repo = store();
        let db = seed(
            &repo,
            &[
                property(2, PropertyKind::Number, 1),
                Property {
                    id: PropertyId(3),
                    db: DatabaseId(1),
                    name: "Status".into(),
                    kind: PropertyKind::Status,
                    // Verbatim: D1 stores the document and never reads inside
                    // it (the option reader is D2's).
                    config: r#"{"options":[{"id":7,"name":"Done","color":"green"}]}"#.into(),
                    ord: OrderKey(OrderKey::FIRST.0 + 2 * 0x100),
                },
            ],
        );
        repo.apply(&[
            Change::DatabaseRenamed {
                id: db.id,
                name: "Tasks and chores".into(),
            },
            Change::PropertyRenamed {
                id: PropertyId(2),
                name: "Estimate".into(),
            },
            Change::ViewAdded(View {
                id: ViewId(2),
                db: db.id,
                name: "Board".into(),
                layout: ViewLayout::Board,
                definition: r#"{"v":1,"groups":[{"property":3}]}"#.into(),
                ord: OrderKey(OrderKey::FIRST.0 + 0x100),
            }),
            // Ahead of the title column on purpose: `ord`, not the id, is the
            // listing order.
            Change::PropertyOrdSet {
                id: PropertyId(3),
                ord: OrderKey(OrderKey::FIRST.0 - 0x10),
            },
        ])
        .unwrap();

        // A row from a build that knows more than this one — a kind and a
        // layout this build has never heard of (ADR-0061 / ADR-0064's folds).
        {
            let conn = repo.database().conn();
            conn.execute(
                "INSERT INTO db_properties (id, db, name, kind, config, ord)
                 VALUES (4, 1, 'Remote', 'quantum', '', 4096)",
                [],
            )
            .unwrap();
            conn.execute("UPDATE db_views SET layout = 'kanban' WHERE id = 2", [])
                .unwrap();
        }

        let catalog = repo.load_databases().unwrap();
        assert_eq!(catalog.databases.len(), 1);
        let loaded = catalog.database(DatabaseId(1)).unwrap();
        assert_eq!(loaded.name, "Tasks and chores", "the rename landed");

        let properties: Vec<_> = catalog.properties_of(DatabaseId(1)).collect();
        assert_eq!(
            properties.len(),
            4,
            "the title column, two columns and the stranger"
        );
        assert_eq!(properties[0].id, PropertyId(3), "ord is the listing order");
        let title = catalog.title_property(DatabaseId(1)).unwrap();
        assert_eq!(title.name, TITLE_PROPERTY_NAME);
        assert_eq!(title.kind, PropertyKind::Title);
        let estimate = properties.iter().find(|p| p.id == PropertyId(2)).unwrap();
        assert_eq!(estimate.name, "Estimate");
        assert_eq!(estimate.kind, PropertyKind::Number);
        let status = properties.iter().find(|p| p.id == PropertyId(3)).unwrap();
        assert_eq!(
            status.config, r#"{"options":[{"id":7,"name":"Done","color":"green"}]}"#,
            "a column's own settings survive byte for byte"
        );
        let stranger = properties.iter().find(|p| p.id == PropertyId(4)).unwrap();
        assert_eq!(
            stranger.kind,
            PropertyKind::Text,
            "an unknown kind opens as text rather than failing the library"
        );

        let views: Vec<_> = catalog.views_of(DatabaseId(1)).collect();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].id, ViewId(1));
        assert_eq!(views[0].layout, ViewLayout::Table);
        assert_eq!(views[0].definition, "", "no rules is the empty document");
        assert_eq!(views[1].name, "Board");
        assert_eq!(
            views[1].layout,
            ViewLayout::Table,
            "an unknown layout opens as a table"
        );
        assert_eq!(views[1].definition, r#"{"v":1,"groups":[{"property":3}]}"#);
    }

    #[test]
    fn a_cell_round_trips_in_every_shape_and_empty_is_the_absence_of_a_row() {
        let repo = store();
        let db = seed(
            &repo,
            &[
                property(2, PropertyKind::Number, 1),
                property(3, PropertyKind::Checkbox, 2),
                property(4, PropertyKind::MultiSelect, 3),
                property(5, PropertyKind::Text, 4),
            ],
        );
        let record = RecordId(7);
        repo.apply(&[Change::RecordCreated(Record::bare(
            record,
            db.id,
            OrderKey::FIRST,
        ))])
        .unwrap();

        // Nothing written: an empty cell is no row at all (ADR-0062) — the
        // question this slice owes an answer to, rather than a blank string.
        assert_eq!(raw_count(&repo, "db_values"), 0);
        assert_eq!(repo.cell(record, PropertyId(1)).unwrap(), CellValue::Empty);

        let set = |property: u64, value: CellValue| Change::CellSet {
            record,
            property: PropertyId(property),
            value,
        };
        repo.apply(&[set(1, CellValue::Text("Write the brief".into()))])
            .unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(1)).unwrap(),
            CellValue::Text("Write the brief".into())
        );

        repo.apply(&[set(2, CellValue::Number(2.5))]).unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Number(2.5)
        );
        // A number is in the `num` column, so SQLite can sort it numerically —
        // the whole reason for the typed columns (ADR-0062).
        {
            let conn = repo.database().conn();
            let (text, num): (String, Option<f64>) = conn
                .query_row(
                    "SELECT text, num FROM db_values WHERE record = 7 AND property = 2",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(text, "");
            assert_eq!(num, Some(2.5));
        }

        repo.apply(&[
            set(3, CellValue::Flag(true)),
            set(4, CellValue::Items(vec!["7".into(), "9".into()])),
        ])
        .unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(3)).unwrap(),
            CellValue::Flag(true)
        );
        assert_eq!(
            repo.cell(record, PropertyId(4)).unwrap(),
            CellValue::Items(vec!["7".into(), "9".into()]),
            "a list keeps its order"
        );
        // A list lives in `db_value_items` and nowhere else: no `db_values` row
        // for it, or an empty list would read back as a blank text cell.
        assert_eq!(raw_count(&repo, "db_value_items"), 2);
        {
            let conn = repo.database().conn();
            let values: i64 = conn
                .query_row(
                    "SELECT count(*) FROM db_values WHERE record = 7 AND property = 4",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(values, 0);
        }

        // A checkbox that is off is a row that says so, not an absent row: the
        // user's click is what put it there.
        repo.apply(&[set(3, CellValue::Flag(false))]).unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(3)).unwrap(),
            CellValue::Flag(false)
        );

        // A second write replaces the first: one cell is one row, and a list is
        // replaced whole rather than appended to.
        repo.apply(&[set(4, CellValue::Items(vec!["1".into()]))])
            .unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(4)).unwrap(),
            CellValue::Items(vec!["1".into()]),
            "the list is what the last write said, not what it accumulated"
        );
        assert_eq!(raw_count(&repo, "db_value_items"), 1);
        // The number column, twice: the second write replaces the first and
        // leaves the text column holding nothing (one cell, one row).
        repo.apply(&[set(2, CellValue::Number(4.0)), set(2, CellValue::Number(5.0))])
            .unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Number(5.0)
        );
        {
            let conn = repo.database().conn();
            let text: String = conn
                .query_row(
                    "SELECT text FROM db_values WHERE record = 7 AND property = 2",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(text, "", "the second write replaced the first value");
        }

        // The shapes agree with the kinds **by contract**: the caller writes the
        // shape its property's kind stores (ADR-0062's columns are chosen by the
        // kind), and validating that here would put a `SELECT` on the cell-write
        // path D6 has to measure. A `Text` written into a *list* column lands in
        // the text column, which a list column's read never looks at — so the
        // cell reads back empty rather than as two half-values. Pinned so the
        // contract is a test and not a hope.
        repo.apply(&[set(4, CellValue::Text("wrong shape".into()))])
            .unwrap();
        assert_eq!(raw_count(&repo, "db_value_items"), 0, "the items were cleared");
        assert_eq!(repo.cell(record, PropertyId(4)).unwrap(), CellValue::Empty);

        // And clearing is deleting: the row is gone, not blank.
        repo.apply(&[
            set(1, CellValue::Empty),
            set(3, CellValue::Empty),
            set(4, CellValue::Items(Vec::new())),
        ])
        .unwrap();
        for property in [1u64, 3, 4] {
            assert_eq!(
                repo.cell(record, PropertyId(property)).unwrap(),
                CellValue::Empty
            );
        }
        assert_eq!(raw_count(&repo, "db_values"), 1, "only the number is left");
        assert_eq!(raw_count(&repo, "db_value_items"), 0);

        // A property that does not exist answers Empty rather than erroring.
        assert_eq!(repo.cell(record, PropertyId(999)).unwrap(), CellValue::Empty);
    }

    #[test]
    fn a_window_read_hands_back_the_window_and_not_the_table() {
        let repo = store();
        let db = seed(&repo, &[property(2, PropertyKind::Text, 1)]);
        let ids = rows(&repo, 100);
        let columns = vec![property(2, PropertyKind::Text, 1)];
        let req = RowRequest {
            db: db.id,
            title: PropertyId(1),
            columns: &columns,
        };
        let geometry = ViewGeometry::new(32.0, 720.0);

        assert_eq!(repo.record_count(db.id).unwrap(), 100);
        let first = repo.record(ids[0]).unwrap().unwrap();
        assert_eq!(first.db, db.id);
        assert_eq!(first.page, None, "a new record is bare (ADR-0063)");
        assert_eq!(first.ord, OrderKey(1 << 32));
        assert_eq!(repo.record(RecordId(404)).unwrap(), None);

        // The projection's number, computed before any row is asked for, and
        // then honoured by SQL: 31 rows of 100 at the top of a 720 px viewport
        // with 32 px rows — D0's geometry, now D1's query.
        let realized = repo.realized_rows(&req, geometry, 0.0).unwrap();
        assert_eq!(realized.total(), 100);
        assert_eq!(realized.window(), RowWindow { start: 0, end: 31 });
        assert_eq!(realized.realized(), 31);
        assert_eq!(realized.rows()[0].record, ids[0].as_u64());
        assert_eq!(realized.rows()[0].title, "Row 0");
        assert_eq!(realized.rows()[30].record, ids[30].as_u64());

        // Scrolled into the middle, the rows that come back are that window's
        // and nobody else's, in listing order — the offset counted rows.
        let middle = crate::core::database::window(100, geometry, 1_600.0);
        let a = repo.window_rows(&req, middle).unwrap();
        let b = repo.window_rows(&req, middle).unwrap();
        assert_eq!(a.len(), middle.len());
        assert_eq!(
            a.iter().map(|r| r.record).collect::<Vec<_>>(),
            (middle.start..middle.end)
                .map(|i| ids[i].as_u64())
                .collect::<Vec<_>>()
        );
        assert_eq!(a, b, "the same window is the same rows");
        assert_eq!(
            a.iter().map(|r| r.title.clone()).collect::<Vec<_>>(),
            (middle.start..middle.end)
                .map(|i| format!("Row {i}"))
                .collect::<Vec<_>>()
        );

        // An offset past the end still lands on a whole screenful, because the
        // projection clamped it before SQL ever saw a number.
        let bottom = crate::core::database::max_scroll_y(100, geometry);
        let tail = repo
            .window_rows(
                &req,
                crate::core::database::window(100, geometry, bottom * 2.0),
            )
            .unwrap();
        assert_eq!(tail.len(), 31);
        assert_eq!(tail[30].record, ids[99].as_u64());

        // A window wider than the table is the table, and a zero-length window
        // asks SQL for nothing at all.
        let all = repo.window_rows(&req, RowWindow { start: 0, end: 100 }).unwrap();
        assert_eq!(all.len(), 100);
        assert!(repo
            .window_rows(&req, RowWindow { start: 0, end: 0 })
            .unwrap()
            .is_empty());

        // The cells arrive with the rows, in column order.
        assert_eq!(all[0].cells, vec!["".to_string()], "no value was written");
        repo.apply(&[Change::CellSet {
            record: ids[0],
            property: PropertyId(2),
            value: CellValue::Text("first".into()),
        }])
        .unwrap();
        let after = repo
            .window_rows(&req, RowWindow { start: 0, end: 1 })
            .unwrap();
        assert_eq!(after[0].cells, vec!["first".to_string()]);
    }

    #[test]
    fn an_empty_database_asks_sql_for_nothing_and_a_list_column_brings_its_items() {
        let repo = store();
        let db = seed(
            &repo,
            &[
                property(2, PropertyKind::MultiSelect, 1),
                property(3, PropertyKind::Checkbox, 2),
            ],
        );
        let columns = vec![
            property(2, PropertyKind::MultiSelect, 1),
            property(3, PropertyKind::Checkbox, 2),
        ];
        let req = RowRequest {
            db: db.id,
            title: PropertyId(1),
            columns: &columns,
        };
        let geometry = ViewGeometry::new(32.0, 720.0);

        // No rows: the count is zero, the window is empty, and the read is a
        // query SQL answers without a row.
        assert_eq!(repo.record_count(db.id).unwrap(), 0);
        let empty = repo.realized_rows(&req, geometry, 0.0).unwrap();
        assert_eq!(empty.total(), 0);
        assert_eq!(empty.window(), RowWindow { start: 0, end: 0 });
        assert_eq!(empty.realized(), 0);
        assert!(repo
            .window_rows(&req, RowWindow { start: 0, end: 31 })
            .unwrap()
            .is_empty());

        let ids = rows(&repo, 3);
        repo.apply(&[
            Change::CellSet {
                record: ids[1],
                property: PropertyId(2),
                value: CellValue::Items(vec!["7".into(), "9".into()]),
            },
            Change::CellSet {
                record: ids[1],
                property: PropertyId(3),
                value: CellValue::Flag(true),
            },
            // A list cell in the same window with more items than the row above
            // must not multiply the window's rows: that is why the items are
            // read in their own query, bounded by the window's own records.
            Change::CellSet {
                record: ids[0],
                property: PropertyId(2),
                value: CellValue::Items(vec!["1".into(), "2".into(), "3".into()]),
            },
            // A checkbox the user turned off is a row that says so, and it
            // paints "No" — while a checkbox nobody touched paints nothing.
            Change::CellSet {
                record: ids[0],
                property: PropertyId(3),
                value: CellValue::Flag(false),
            },
        ])
        .unwrap();

        let window = repo.window_rows(&req, RowWindow { start: 0, end: 3 }).unwrap();
        assert_eq!(window.len(), 3, "three records, not seven items");
        assert_eq!(
            window[0].cells,
            vec!["1, 2, 3".to_string(), "No".to_string()]
        );
        assert_eq!(window[1].cells, vec!["7, 9".to_string(), "Yes".to_string()]);
        assert_eq!(window[2].cells, vec!["".to_string(), "".to_string()]);

        // Empty is empty for a list too, and the painted form of one is its ids
        // until D2 reads the option names out of the column's config.
        assert_eq!(repo.cell(ids[2], PropertyId(2)).unwrap(), CellValue::Empty);
        assert_eq!(
            CellValue::Items(vec!["7".into(), "9".into()]).display(),
            "7, 9"
        );
    }

    #[test]
    fn a_records_title_lives_on_its_page_or_in_its_own_value_row() {
        let repo = store();
        let db = seed(&repo, &[]);
        let record = RecordId(7);
        repo.apply(&[
            Change::RecordCreated(Record::bare(record, db.id, OrderKey::FIRST)),
            Change::CellSet {
                record,
                property: PropertyId(1),
                value: CellValue::Text("Bare and named".into()),
            },
        ])
        .unwrap();
        assert_eq!(
            repo.record_title(record).unwrap().as_deref(),
            Some("Bare and named")
        );

        // Opening the row: the page arrives, the title moves out of the value
        // row onto the page, and the value row is cleared — one batch, so one
        // undo step (ADR-0063). The read finds the title wherever its one home
        // is, in the same query as everything else.
        repo.apply(&[
            Change::PageCreated(page(50, "Opened")),
            Change::RecordPageSet {
                id: record,
                page: Some(PageId(50)),
            },
            Change::CellSet {
                record,
                property: PropertyId(1),
                value: CellValue::Empty,
            },
            Change::PageTitleSet {
                id: PageId(50),
                title: "Bare and named".into(),
            },
        ])
        .unwrap();
        assert_eq!(
            repo.record_title(record).unwrap().as_deref(),
            Some("Bare and named")
        );
        assert_eq!(
            repo.cell(record, PropertyId(1)).unwrap(),
            CellValue::Empty,
            "the title is not in two places"
        );
        // The page's own title can change without the record's read learning
        // anything new: there is no copy to update.
        repo.apply(&[Change::PageTitleSet {
            id: PageId(50),
            title: "Renamed on the page".into(),
        }])
        .unwrap();
        assert_eq!(
            repo.record_title(record).unwrap().as_deref(),
            Some("Renamed on the page")
        );

        // The inverse ("Turn into a plain record"): the title moves back and
        // the pointer is cleared. The page stays in the tree — the user made it.
        repo.apply(&[
            Change::PageTitleSet {
                id: PageId(50),
                title: String::new(),
            },
            Change::RecordPageSet { id: record, page: None },
            Change::CellSet {
                record,
                property: PropertyId(1),
                value: CellValue::Text("Renamed on the page".into()),
            },
        ])
        .unwrap();
        assert_eq!(
            repo.record_title(record).unwrap().as_deref(),
            Some("Renamed on the page")
        );
        assert!(repo.record(record).unwrap().unwrap().page.is_none());
        assert_eq!(repo.load().unwrap().pages.len(), 1, "the page is still there");
    }

    #[test]
    fn the_title_column_reads_through_the_same_coalesce_as_the_row() {
        let repo = store();
        let db = seed(&repo, &[property(2, PropertyKind::Text, 1)]);
        // The view shows the title column *and* another one — what a first
        // table view looks like.
        let columns = vec![
            db.title_property(PropertyId(1)),
            property(2, PropertyKind::Text, 1),
        ];
        let req = RowRequest {
            db: db.id,
            title: PropertyId(1),
            columns: &columns,
        };
        let ids = rows(&repo, 1);
        repo.apply(&[
            Change::PageCreated(page(50, "Page title")),
            Change::RecordPageSet {
                id: ids[0],
                page: Some(PageId(50)),
            },
            Change::CellSet {
                record: ids[0],
                property: PropertyId(1),
                value: CellValue::Empty,
            },
        ])
        .unwrap();

        let window = repo.window_rows(&req, RowWindow { start: 0, end: 1 }).unwrap();
        assert_eq!(window[0].title, "Page title");
        assert_eq!(
            window[0].cells[0], "Page title",
            "a visible title column says the same thing as the row's title"
        );
        assert_eq!(window[0].cells[1], "");
    }

    #[test]
    fn deleting_a_property_takes_its_values_and_leaves_the_other_columns() {
        let repo = store();
        let db = seed(
            &repo,
            &[
                property(2, PropertyKind::Text, 1),
                property(3, PropertyKind::MultiSelect, 2),
            ],
        );
        let ids = rows(&repo, 2);
        repo.apply(&[
            Change::CellSet {
                record: ids[0],
                property: PropertyId(2),
                value: CellValue::Text("doomed".into()),
            },
            Change::CellSet {
                record: ids[0],
                property: PropertyId(3),
                value: CellValue::Items(vec!["9".into()]),
            },
        ])
        .unwrap();
        assert_eq!(raw_count(&repo, "db_values"), 3, "two titles and one text");

        repo.apply(&[
            Change::PropertyDeleted { id: PropertyId(2) },
            Change::PropertyDeleted { id: PropertyId(3) },
        ])
        .unwrap();
        assert_eq!(
            raw_count(&repo, "db_values"),
            2,
            "the titles are all that is left"
        );
        assert_eq!(raw_count(&repo, "db_value_items"), 0);
        let catalog = repo.load_databases().unwrap();
        assert_eq!(
            catalog.properties_of(db.id).count(),
            1,
            "only the title column is left"
        );
        // The record is untouched: deleting a column is not deleting a row.
        assert_eq!(repo.record_count(db.id).unwrap(), 2);
        assert_eq!(repo.record_title(ids[0]).unwrap().as_deref(), Some("Row 0"));
    }

    #[test]
    fn deleting_a_database_takes_its_schema_and_rows_but_leaves_the_pages() {
        let repo = store();
        let db = seed(&repo, &[property(2, PropertyKind::Text, 1)]);
        let ids = rows(&repo, 2);
        repo.apply(&[
            Change::PageCreated(page(50, "Face of a record")),
            Change::RecordPageSet {
                id: ids[0],
                page: Some(PageId(50)),
            },
            Change::PageCreated(page(51, "An ordinary page")),
            Change::CellSet {
                record: ids[1],
                property: PropertyId(2),
                value: CellValue::Text("a value".into()),
            },
        ])
        .unwrap();

        repo.apply(&[Change::DatabaseDeleted { id: db.id }]).unwrap();
        let catalog = repo.load_databases().unwrap();
        assert!(catalog.databases.is_empty());
        assert_eq!(catalog.properties.len(), 0, "its columns went with it");
        assert_eq!(catalog.views.len(), 0);
        assert_eq!(repo.record_count(db.id).unwrap(), 0);
        assert_eq!(repo.record(ids[0]).unwrap(), None);
        for table in ["db_values", "db_value_items"] {
            assert_eq!(raw_count(&repo, table), 0, "{table} cascaded");
        }
        // A page is owned by a record, never by the database (ADR-0063): both
        // pages are still there, including the one a record was the face of.
        let state = repo.load().unwrap();
        assert_eq!(state.pages.len(), 2);
    }

    #[test]
    fn a_page_is_the_face_of_at_most_one_record() {
        let repo = store();
        let db = seed(&repo, &[]);
        let ids = rows(&repo, 2);
        repo.apply(&[Change::PageCreated(page(50, "One page"))]).unwrap();
        repo.apply(&[Change::RecordPageSet {
            id: ids[0],
            page: Some(PageId(50)),
        }])
        .unwrap();

        // `UNIQUE (page)` is what makes the ownership mutual (ADR-0063), and it
        // is SQL that refuses the second claim, not the app remembering to.
        let refused = repo.apply(&[Change::RecordPageSet {
            id: ids[1],
            page: Some(PageId(50)),
        }]);
        assert!(
            matches!(refused, Err(StorageError::Sql(_))),
            "expected a unique violation, got {refused:?}"
        );
        assert_eq!(repo.record(ids[1]).unwrap().unwrap().page, None);
        // Bare records do not queue behind that constraint: many NULLs are legal
        // under it, which is what lazy page creation rests on.
        assert_eq!(raw_count(&repo, "db_records"), 2);
        let _ = db;
    }

    #[test]
    fn a_kind_change_and_a_column_move_leave_the_values_where_they_are() {
        let repo = store();
        let db = seed(&repo, &[property(2, PropertyKind::Text, 1)]);
        let ids = rows(&repo, 1);
        repo.apply(&[Change::CellSet {
            record: ids[0],
            property: PropertyId(2),
            value: CellValue::Text("12".into()),
        }])
        .unwrap();

        // A kind change is not a conversion: the value stays in the `text`
        // column it was written to, and converting a column's values is D2's
        // per-type work rather than a side effect of this change.
        repo.apply(&[
            Change::PropertyKindSet {
                id: PropertyId(2),
                kind: PropertyKind::Number,
            },
            Change::PropertyOrdSet {
                id: PropertyId(2),
                ord: OrderKey::FIRST,
            },
        ])
        .unwrap();
        {
            let conn = repo.database().conn();
            let (text, num): (String, Option<f64>) = conn
                .query_row(
                    "SELECT text, num FROM db_values WHERE record = ?1 AND property = 2",
                    params![ids[0].as_u64() as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!((text.as_str(), num), ("12", None));
        }
        let catalog = repo.load_databases().unwrap();
        let moved = catalog
            .properties_of(db.id)
            .find(|p| p.id == PropertyId(2))
            .unwrap();
        assert_eq!(moved.kind, PropertyKind::Number);
        assert_eq!(moved.ord, OrderKey::FIRST, "the column moved to the front");
    }

    #[test]
    fn a_write_against_an_id_that_is_not_there_fails_loudly_and_takes_its_batch_with_it() {
        let repo = store();
        let db = seed(&repo, &[]);
        // Every update and delete goes through `require_hit`: silently dropping
        // a change would desynchronize whatever book the caller keeps.
        assert!(matches!(
            repo.apply(&[Change::RecordDeleted { id: RecordId(404) }]),
            Err(StorageError::Sql(_))
        ));
        assert!(matches!(
            repo.apply(&[Change::ViewDeleted { id: ViewId(404) }]),
            Err(StorageError::Sql(_))
        ));
        assert!(matches!(
            repo.apply(&[Change::DatabaseDeleted {
                id: DatabaseId(404)
            }]),
            Err(StorageError::Sql(_))
        ));
        assert!(repo
            .apply(&[Change::DatabaseRenamed {
                id: db.id,
                name: "kept".into()
            }])
            .is_ok());

        // The rest of a failed batch is rolled back with it: one `apply` is one
        // transaction (SPEC §十八).
        let failed = repo.apply(&[
            Change::ViewAdded(View {
                id: ViewId(9),
                db: db.id,
                name: "Second".into(),
                layout: ViewLayout::List,
                definition: String::new(),
                ord: OrderKey(OrderKey::FIRST.0 + 0x100),
            }),
            Change::ViewDeleted { id: ViewId(404) },
        ]);
        assert!(failed.is_err());
        let catalog = repo.load_databases().unwrap();
        assert_eq!(catalog.database(db.id).unwrap().name, "kept");
        assert!(
            catalog.views_of(db.id).all(|v| v.id != ViewId(9)),
            "the view the batch inserted did not survive the batch that failed"
        );
    }
}

// ─── the measurement ────────────────────────────────────────────────────────
//
// What D1 owes: 10 000 records in the store, one window read at three scroll
// positions, and the same rows without a window as the control. The heap
// counter is D0's (`core::database::probe`), shared because a test binary has
// exactly one global allocator.

#[cfg(test)]
mod probe {
    use super::*;
    use crate::core::database::probe::{counters, measure};
    use crate::core::persistence::{Change, Repository};
    use std::time::Instant;

    const ROWS: usize = 10_000;
    const BATCH: usize = 500;

    fn property(id: u64, kind: PropertyKind, ordinal: u64) -> Property {
        Property {
            id: PropertyId(id),
            db: DatabaseId(1),
            name: format!("P{id}"),
            kind,
            config: String::new(),
            ord: OrderKey(OrderKey::FIRST.0 + ordinal * 0x100),
        }
    }

    #[test]
    #[ignore = "prints a measurement; run with --release --lib -- --ignored --nocapture"]
    fn a_window_read_costs_its_window_and_the_table_costs_the_table() {
        let dir = crate::testing::ScratchDir::new("db-window");
        let path = dir.join("quire.db");
        let repo = SqliteRepository::open(&path).unwrap();

        // A database with a title column and four more: one of each shape a
        // window read has to handle.
        let db = Database::new(DatabaseId(1), "Tasks");
        let title = db.title_property(PropertyId(1));
        let columns = vec![
            property(2, PropertyKind::Text, 1),
            property(3, PropertyKind::Number, 2),
            property(4, PropertyKind::Checkbox, 3),
            property(5, PropertyKind::MultiSelect, 4),
        ];
        let mut schema = vec![
            Change::DatabaseCreated(db.clone()),
            Change::PropertyAdded(title.clone()),
            Change::ViewAdded(db.first_view(ViewId(1))),
        ];
        schema.extend(columns.iter().cloned().map(Change::PropertyAdded));
        repo.apply(&schema).unwrap();

        let before = counters::process_bytes();

        // 10 000 rows, each with a title and a value in every column: 60 000
        // changes in 20 transactions — what a bulk import looks like when it
        // rides the app's own change path.
        let started = Instant::now();
        let mut next = 1u64;
        for batch in 0..(ROWS / BATCH) {
            let mut changes = Vec::with_capacity(BATCH * 6);
            for i in 0..BATCH {
                let index = batch * BATCH + i;
                let record = RecordId(next);
                next += 1;
                changes.push(Change::RecordCreated(Record::bare(
                    record,
                    db.id,
                    OrderKey(((index as u64) + 1) << 32),
                )));
                changes.push(Change::CellSet {
                    record,
                    property: title.id,
                    value: CellValue::Text(format!("Note {index}")),
                });
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(2),
                    value: CellValue::Text(format!("text {index}")),
                });
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(3),
                    value: CellValue::Number(index as f64 * 1.5),
                });
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(4),
                    value: CellValue::Flag(index % 2 == 0),
                });
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(5),
                    value: CellValue::Items(vec![
                        format!("{}", index % 7),
                        format!("{}", index % 3),
                    ]),
                });
            }
            repo.apply(&changes).unwrap();
        }
        let insert_ms = started.elapsed().as_secs_f64() * 1e3;
        let after = counters::process_bytes();

        let req = RowRequest {
            db: db.id,
            title: title.id,
            columns: &columns,
        };
        let total = repo.record_count(db.id).unwrap();
        assert_eq!(total, ROWS, "every row the insert wrote is in the count");
        let geometry = ViewGeometry::new(32.0, 720.0);

        // Warm the page cache and the statement machinery with one tiny read,
        // so the measured window is not the one paying for first touch.
        assert_eq!(
            repo.window_rows(&req, RowWindow { start: 0, end: 1 })
                .unwrap()
                .len(),
            1
        );

        let top = crate::core::database::window(total, geometry, 0.0);
        let mut readings = Vec::new();
        for scroll in [
            0.0f32,
            160_000.0,
            crate::core::database::max_scroll_y(total, geometry),
        ] {
            let window = crate::core::database::window(total, geometry, scroll);
            let started = Instant::now();
            let rows = repo.window_rows(&req, window).unwrap();
            let us = started.elapsed().as_secs_f64() * 1e6;
            assert_eq!(
                rows.len(),
                window.len(),
                "SQL returned the window and no more"
            );
            readings.push((window, us, rows.len()));
        }

        // The projection and the query together: D0's arithmetic deciding what
        // SQL is asked for, in one call.
        let started = Instant::now();
        let realized = repo.realized_rows(&req, geometry, 0.0).unwrap();
        let realized_us = started.elapsed().as_secs_f64() * 1e6;
        assert_eq!(realized.total(), ROWS);
        assert_eq!(realized.realized(), 31);

        // The control arm: the same query without the window. Read once to grow
        // SQLite's own buffers, then measured — so the bytes below are the rows
        // and not the page cache that holds them.
        let _ = repo.unwindowed_rows(&req).unwrap();
        let started = Instant::now();
        let (all, all_bytes) = measure(|| repo.unwindowed_rows(&req).unwrap());
        let all_ms = started.elapsed().as_secs_f64() * 1e3;
        let (window_rows, window_bytes) = measure(|| repo.window_rows(&req, top).unwrap());
        assert_eq!(all.len(), ROWS);
        assert_eq!(window_rows.len(), top.len());

        // Why the last window costs what it costs: `OFFSET` walks the rows it
        // skips, and a scroll is one of these per frame. Two readings isolate
        // it — the same window as a bare index walk with no joins, and the same
        // window asked for by *cursor* (one key, then a range scan) — so the
        // number is a cause and not just a time.
        let (walk_us, keyset_us, keyset_rows) = {
            let conn = repo.database().conn();
            let started = Instant::now();
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM db_records WHERE db = ?1 ORDER BY ord, id
                     LIMIT ?2 OFFSET ?3",
                )
                .unwrap();
            let ids: Vec<i64> = stmt
                .query_map(
                    params![1i64, top.len() as i64, top.start as i64],
                    |r| r.get(0),
                )
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let walk_us = started.elapsed().as_secs_f64() * 1e6;
            assert_eq!(ids.len(), top.len());

            let (ord, cursor) = conn
                .query_row(
                    "SELECT ord, id FROM db_records WHERE db = ?1 ORDER BY ord, id
                     LIMIT 1 OFFSET ?2",
                    params![1i64, top.start as i64 - 1],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .unwrap();
            // The same statement with its window clause replaced by a cursor
            // predicate: the row's own key, not a count of the rows before it.
            let keyset = row_query(&req, None).replace(
                "ORDER BY r.ord, r.id",
                "AND (r.ord, r.id) > (?7, ?8) ORDER BY r.ord, r.id LIMIT ?9",
            );
            let started = Instant::now();
            let mut stmt = conn.prepare(&keyset).unwrap();
            let rows: Vec<i64> = stmt
                .query_map(
                    params![1i64, 2i64, 3i64, 4i64, 5i64, 1i64, ord, cursor, top.len() as i64],
                    |r| r.get(0),
                )
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let keyset_us = started.elapsed().as_secs_f64() * 1e6;
            (walk_us, keyset_us, rows.len())
        };
        assert_eq!(keyset_rows, top.len());

        // The evidence that the window is a query and not a truncation: the
        // statement SQLite is handed, the pair at its end, and the plan it
        // chose for it.
        let (query, binds, plan) = {
            let conn = repo.database().conn();
            let query = row_query(&req, Some(top));
            let binds = row_binds(&req, Some(top));
            let mut stmt = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap();
            let plan: Vec<String> = stmt
                .query_map(params_from_iter(binds.iter().copied()), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            (query, binds, plan)
        };

        let rows_mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        println!(
            "database window probe: {ROWS} records x {} columns, window geometry {geometry:?}",
            columns.len() + 1
        );
        println!(
            "  insert: {ROWS} records + {} cells in {BATCH}-change batches: {insert_ms:.1} ms \
             ({:.2} us/row, {:.0} changes/s)",
            (columns.len() + 1) * ROWS,
            insert_ms * 1000.0 / ROWS as f64,
            ((columns.len() + 1) * ROWS) as f64 / (insert_ms / 1000.0)
        );
        println!("  count: {total} rows (SELECT count(*) — one index walk, no row objects)");
        for (window, us, rows) in &readings {
            println!(
                "  window {}..{} = {rows} rows in {us:.1} us  (LIMIT {} OFFSET {})",
                window.start,
                window.end,
                window.len(),
                window.start
            );
        }
        println!(
            "  projection + query at scroll 0: {realized_us:.1} us for {} rows",
            realized.realized()
        );
        println!(
            "  control: all {ROWS} rows in {all_ms:.1} ms, {all_bytes} B ({:.2} MB) held",
            rows_mb(all_bytes)
        );
        println!(
            "  offset: the same window with no joins is {walk_us:.1} us; by cursor (one key, \
             a range scan) it is {keyset_us:.1} us for the same {} rows",
            top.len()
        );
        println!(
            "  window: {} rows in {} B; the table's rows are {:.0}x that ({:.2} MB vs {:.1} KB)",
            window_rows.len(),
            window_bytes,
            all_bytes as f64 / window_bytes.max(1) as f64,
            rows_mb(all_bytes),
            window_bytes as f64 / 1024.0
        );
        match (before, after) {
            (Some((ws0, priv0)), Some((ws1, priv1))) => println!(
                "  process while inserting: working set {:.1} -> {:.1} MB, private {:.1} -> {:.1} MB",
                rows_mb(ws0),
                rows_mb(ws1),
                rows_mb(priv0),
                rows_mb(priv1)
            ),
            _ => println!("  process: not readable on this platform"),
        }
        println!("  query: {query}");
        println!("  binds (?1 = title, then the columns, the database, then the window): {binds:?}");
        println!("  plan:");
        for line in &plan {
            println!("    {line}");
        }
        let (ws_mb, priv_mb) = match after {
            Some((ws, priv_bytes)) => (rows_mb(ws), rows_mb(priv_bytes)),
            None => (0.0, 0.0),
        };
        println!(
            "{{\"label\":\"track3-d1-window\",\"date\":\"2026-09-22\",\
             \"harness\":\"cargo test --release --lib -- --ignored --nocapture\",\
             \"records\":{ROWS},\"properties\":{},\"insert_ms\":{insert_ms:.1},\
             \"insert_us_per_row\":{:.2},\"window_rows\":{},\"window_fetch_limit\":{},\
             \"window_fetch_offset\":{},\"window_read_us\":{:.1},\"mid_rows\":{},\
             \"mid_read_us\":{:.1},\"bottom_rows\":{},\"bottom_read_us\":{:.1},\
             \"realized_read_us\":{:.1},\"offset_walk_us\":{:.1},\"cursor_read_us\":{:.1},\"all_rows\":{},\"all_read_ms\":{all_ms:.1},\
             \"heap_window_bytes\":{window_bytes},\"heap_all_rows_bytes\":{all_bytes},\
             \"heap_ratio\":{:.0},\"process_working_set_mb\":{ws_mb:.1},\
             \"process_private_mb\":{priv_mb:.1}}}",
            columns.len() + 1,
            insert_ms * 1000.0 / ROWS as f64,
            readings[0].2,
            readings[0].0.len(),
            readings[0].0.start,
            readings[0].1,
            readings[1].2,
            readings[1].1,
            readings[2].2,
            readings[2].1,
            realized_us,
            walk_us,
            keyset_us,
            all.len(),
            all_bytes as f64 / window_bytes.max(1) as f64,
        );
    }
}
