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
//
// D2 adds the property system's SQL half, and two of its three pieces are
// shapes rather than code:
//
//   3. **A cell is painted through its column** (`core::database_property`):
//      an option id becomes a name, a number goes through its format, a file id
//      through the one query that names attachments for this window. The store
//      owns exactly the part the core cannot do — the query — and hands the
//      answer over.
//   4. **The sort is an `ORDER BY` in the statement** (§三十九's red line:
//      nothing in the UI filters or sorts). `RowRequest::sort` is compiled into
//      the same query the window already runs, so the rows that come back are
//      the sorted window's rows and no Rust `sort_by` exists anywhere on the
//      read path. `row_query` is where the comparison's *column* is chosen, and
//      that choice is what makes `2` sort before `10`.
//   5. **The derived kinds read the record's own columns** (ADR-0068): a
//      `created time` / `last edited time` cell is `db_records.created` /
//      `.edited`, never a `db_values` row, and the write path is what stamps
//      them.
//   6. **The view's rules compile into the same statement** (D4, ADR-0076): the
//      filter tree, the sort list and the group are turned into this
//      statement's `WHERE` / `ORDER BY` / group predicate by
//      `storage::database_query` — the module that owns every piece of SQL
//      text the rules produce — and this file is the only place that executes
//      it. The red line (「filter / sort 在 SQL 侧完成，不在 UI 侧过滤」) is
//      that split: the query is built with the rules *inside* it, so the
//      window slices the filtered order and no Rust code ever holds rows to
//      throw them away.

use std::collections::{BTreeSet, HashMap};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
// The test/probe helpers `EXPLAIN` a statement with its binds, which are
// `Value`s now that a filter's binds are heterogeneous.
#[cfg(test)]
use rusqlite::types::Value;

use crate::core::database::{
    CellValue, Database, DatabaseCatalog, DatabaseId, Property, PropertyId, PropertyKind,
    RealizedRows, Record, RecordId, RecordTimestamps, RowRequest, RowView, RowWindow, ValueKind,
    View, ViewGeometry, ViewId,
};
// The tests compile requests with sort terms; the read path's own ORDER BY
// lives in `database_query` now.
#[cfg(test)]
use crate::core::database::SortSpec;
use crate::core::database_view::{GroupKey, GroupSpec, RecordPages};
use crate::core::persistence::StorageError;
use crate::core::types::{OrderKey, PageId};

use super::database::{ord_from_db, ord_to_db};
use super::database_query::{count_query, group_query, range_query, row_query_in_group, row_query_plan};
use super::repository::{require_hit, SqliteRepository};

/// The one clock in the database layer (ADR-0068): SQLite's own, read as the
/// stored date shape — `YYYY-MM-DDTHH:MM`, local wall time, fixed width. A
/// record's `created` / `edited` are stamped by statements that use this
/// expression, never by a caller passing a time in: the write path is the only
/// thing that can keep an instant honest, so it is the only thing that writes
/// one. `localtime` is the same clock the user's files carry; `COALESCE` is
/// there because a machine whose zone SQLite cannot read returns NULL, and a
/// NULL in a `NOT NULL` column is a failed write rather than a blank stamp.
const NOW: &str = "COALESCE(strftime('%Y-%m-%dT%H:%M','now','localtime'), strftime('%Y-%m-%dT%H:%M','now'))";

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
                .prepare("SELECT id, name, template FROM databases ORDER BY id")
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Database {
                        id: DatabaseId(r.get::<_, i64>(0)? as u64),
                        name: r.get(1)?,
                        // v20 (ADR-0086). `''` is "no template" and the
                        // document is stored verbatim: parsing is the caller's
                        // (`database_template::cells_of`), and a document this
                        // build cannot read folds there, not here.
                        template: r.get(2)?,
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

    /// A record's two instants, as step 17 stores them (ADR-0068), or `None`
    /// when there is no such record. `""` is a record that predates the step —
    /// an upgrade invents no birthdays — and paints as an empty cell.
    ///
    /// Deliberately **not** a field of `Record`: `Record` is what a `Change`
    /// carries, and a change that could name a birthday would be a caller that
    /// can invent one. The store is the write path, so the store is what stamps
    /// them (see [`NOW`]).
    pub fn record_timestamps(
        &self,
        id: RecordId,
    ) -> Result<Option<RecordTimestamps>, StorageError> {
        read_record_timestamps(&self.database().conn(), id)
    }

    /// One cell, in the shape its column stores (ADR-0062): the `text` column,
    /// the `num` column, the `flag` column, or `db_value_items`. A property
    /// that does not exist, a column that stores nothing (`formula` / `rollup`
    /// / `relation`), and a cell nobody ever wrote all answer `Empty` — which is
    /// this design's single representation of "no value".
    ///
    /// The two derived kinds (ADR-0068) are read from the record's own row and
    /// never from `db_values`. That is what makes "no double write" structural
    /// rather than promised: a `db_values` row written at a derived column by a
    /// caller that ignored the contract is not a value any read path consults.
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
        let kind = PropertyKind::from_stored(&kind);
        if kind.is_derived() {
            // The guard is already held: the read goes through the free
            // function rather than back through `self`, because this mutex is
            // not reentrant (a hung test is what taught this slice that).
            return derived_cell(&conn, record, kind);
        }
        match kind.value_kind() {
            ValueKind::Computed | ValueKind::Derived => Ok(CellValue::Empty),
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

    /// One column's stored values for **every record of one database**, keyed
    /// by record — the read D6's Markdown export preloads its formula
    /// dependencies from. The window read has no need of it (a formula on 31
    /// realized rows point-reads its dependencies through `cell`, ADR-0083);
    /// the export renders the *whole* view, and one indexed sweep per
    /// dependency column is the honest shape there — the same trade
    /// `unwindowed_rows` makes, for the same reason.
    ///
    /// A `formula` / `rollup` / `relation` column answers an empty map (it
    /// stores nothing, ADR-0062), and a missing value simply does not appear in
    /// the map — absence is empty, as everywhere else in this layer.
    pub fn column_values(
        &self,
        db: DatabaseId,
        property: PropertyId,
    ) -> Result<std::collections::HashMap<u64, CellValue>, StorageError> {
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
            return Ok(std::collections::HashMap::new());
        };
        let kind = PropertyKind::from_stored(&kind);
        if matches!(kind.value_kind(), ValueKind::Computed | ValueKind::Derived) {
            return Ok(std::collections::HashMap::new());
        }
        let mut statement = conn
            .prepare(
                "SELECT v.record, v.text, v.num, v.flag FROM db_values v
                 JOIN db_records r ON r.id = v.record
                 WHERE r.db = ?1 AND v.property = ?2",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map(
                params![db.as_u64() as i64, property.as_u64() as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .map_err(sql)?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (record, text, num, flag) = row.map_err(sql)?;
            let value = match kind.value_kind() {
                ValueKind::Text => CellValue::Text(text.unwrap_or_default()),
                ValueKind::Number => match num {
                    Some(num) => CellValue::Number(num),
                    // A `NULL` num is empty, never 0 — the same rule `cell`
                    // states for one row (ADR-0062's single "no value").
                    None => CellValue::Empty,
                },
                ValueKind::Flag => CellValue::Flag(flag.unwrap_or(0) != 0),
                // The list kinds are not fetched by this sweep (their values
                // live in `db_value_items`, one row per item): a formula that
                // names one reads it as empty (`core::database_formula::val_of`),
                // so a map that pretends otherwise would disagree with the
                // window path.
                _ => CellValue::Empty,
            };
            out.insert(record, value);
        }
        Ok(out)
    }

    /// **The workspace's local member list** — SPEC §三十九's `person` 降级, and
    /// ADR-0071's shape for it: no accounts, no member table, no ids. A person
    /// is a name in a text cell, and the list a picker offers is the distinct
    /// names the file already holds, read out of the values rather than kept
    /// beside them (a second copy is a second thing to go stale).
    ///
    /// The predicate is the **stored** kind string, which is why this is a
    /// query and not a walk over the catalog: `PropertyKind::from_stored` folds
    /// `person` to `text` (ADR-0061 — this build has no account model and no
    /// person-specific behaviour to hang on a variant), while SQL still sees the
    /// word the file was written with. A file this build created has no such
    /// column, so an empty answer is the honest one here; a file written by a
    /// build that knows `person` gets its names back, and the day this build
    /// grows the variant the same query starts answering for its own cells.
    pub fn workspace_people(&self) -> Result<Vec<String>, StorageError> {
        let conn = self.database().conn();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT v.text FROM db_values v
                   JOIN db_properties p ON p.id = v.property
                  WHERE p.kind = 'person' AND v.text <> ''
                  ORDER BY v.text COLLATE NOCASE, v.text",
            )
            .map_err(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(sql)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql)
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
    ///
    /// D4 narrows the count instead of the rows: a request carrying a filter is
    /// counted by [`Self::filtered_count`] — `COUNT(*)` over the same
    /// predicate the row read runs — so the window is a slice of the *filtered*
    /// table. Filter 10 000 rows down to 3 and this realizes 3 rows, because 3
    /// is what the window arithmetic was handed; the unfiltered count would
    /// have made this function hold the table in order to discard it.
    pub fn realized_rows(
        &self,
        req: &RowRequest<'_>,
        geometry: ViewGeometry,
        scroll_y: f32,
    ) -> Result<RealizedRows, StorageError> {
        let total = if req.filter.is_some() {
            self.filtered_count(req)?
        } else {
            self.record_count(req.db)?
        };
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

    /// How many rows the view's rules admit — `COUNT(*)` over the same `FROM`
    /// and `WHERE` the row read runs (`database_query::count_query`), the
    /// number the window is computed from, computed in SQL **before** any row
    /// is asked for. The contract the unified tests must pin: filter a
    /// 10 000-row database down to 3 rows and the window realizes 3 rows,
    /// because 3 is what `core::database::window` was handed — the alternative
    /// (fetch the table, filter in Rust) is the defect the red line names, and
    /// D4's对照 probe measures both so the difference has a number.
    pub fn filtered_count(&self, req: &RowRequest<'_>) -> Result<usize, StorageError> {
        let conn = self.database().conn();
        let plan = count_query(req);
        let n: i64 = conn
            .query_row(
                &plan.sql,
                params_from_iter(plan.binds.iter().cloned()),
                |r| r.get(0),
            )
            .map_err(sql)?;
        Ok(count(n))
    }

    /// One view's group list — `(key, count)` per distinct value of the group
    /// column among the rows the filter admits, normalized into
    /// [`GroupKey`]s. **Bounded by the groupable kinds** (ADR-0076: checkbox /
    /// select / status), so this is the list of *headers* and never a second
    /// copy of the table: a text column's 10 000 distinct values are exactly
    /// why those kinds do not group.
    ///
    /// No order is imposed here: the header order is the schema's own option
    /// order (ADR-0061), which lives in the column's config JSON where SQL
    /// cannot see it, so the caller sorts these few rows in Rust. That is not
    /// the red line bent — the *rows* are SQL's, each group's slice comes from
    /// its own ordered query; the handful of headers are ordered by the same
    /// config the option cells are painted from.
    pub fn group_counts(
        &self,
        req: &RowRequest<'_>,
        spec: &GroupSpec,
    ) -> Result<Vec<(GroupKey, usize)>, StorageError> {
        let conn = self.database().conn();
        let plan = group_query(req, spec);
        let mut stmt = conn.prepare(&plan.sql).map_err(sql)?;
        let mut rows = stmt
            .query(params_from_iter(plan.binds.iter().cloned()))
            .map_err(sql)?;
        let mut out: Vec<(GroupKey, usize)> = Vec::new();
        while let Some(row) = rows.next().map_err(sql)? {
            let n: i64 = row.get(1).map_err(sql)?;
            // The key is normalized here, at the only place that has seen the
            // raw column: SQL's NULL and '' are one group, and a checkbox's 0
            // and its absence are one group (an untouched box is unchecked).
            let key = match spec.kind {
                PropertyKind::Checkbox => GroupKey::of_flag(row.get(0).map_err(sql)?),
                _ => GroupKey::of_text(row.get::<_, Option<String>>(0).map_err(sql)?.as_deref()),
            };
            match out.iter_mut().find(|(k, _)| *k == key) {
                Some((_, total)) => *total += count(n),
                None => out.push((key, count(n))),
            }
        }
        Ok(out)
    }

    /// One group's slice of the window — `skip` of the group's own leading rows
    /// and `len` after them, in the view's order. The `LIMIT`/`OFFSET` apply
    /// *inside the group* (the group's predicate is in the `WHERE`), which is
    /// what keeps a grouped 10 000-row database as virtualized as an ungrouped
    /// one: the rows that exist as objects are the viewport's, wherever they
    /// sit relative to their group's header.
    pub fn window_rows_in_group(
        &self,
        req: &RowRequest<'_>,
        spec: &GroupSpec,
        key: &GroupKey,
        skip: usize,
        len: usize,
    ) -> Result<Vec<RowView>, StorageError> {
        run_row_query(
            &self.database().conn(),
            row_query_in_group(req, spec, key, skip, len),
            req,
        )
    }

    /// The span one column takes across the rows the filter admits, as the
    /// stored text at each end — D5's timeline asks this once per refresh
    /// (`database_query::range_query`: two aggregates over the same `FROM`/
    /// `WHERE` the row read runs) to size its axis, and no row is taken to
    /// answer it. `None` when no admitted row holds a value at all: an empty
    /// axis is a fact the view draws, not an error.
    ///
    /// The values come back as stored text, not parsed dates: the caller (which
    /// knows the column's kind) decides what they mean, and the store stays the
    /// one place that runs SQL and the only place that does not interpret it.
    pub fn column_bounds(
        &self,
        req: &RowRequest<'_>,
        property: PropertyId,
        kind: PropertyKind,
    ) -> Result<Option<(String, String)>, StorageError> {
        let conn = self.database().conn();
        let plan = range_query(req, property, kind);
        let row: (Option<String>, Option<String>) = conn
            .query_row(
                &plan.sql,
                params_from_iter(plan.binds.iter().cloned()),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql)?;
        // An empty text is the pre-ADR-0068 stamp shape, not a bound; a min
        // that comes back `''` means nothing real was stored.
        match (row.0.filter(|t| !t.is_empty()), row.1.filter(|t| !t.is_empty())) {
            (Some(low), Some(high)) => Ok(Some((low, high))),
            _ => Ok(None),
        }
    }

    /// The highest `ord` in one database, or `None` when it has no rows — what
    /// the "new row" path asks for the key of the row it is about to append.
    ///
    /// `MAX` over the `(db, ord)` index the record table already carries, so it
    /// is the index's rightmost leaf rather than a table scan — which is the
    /// point: the app is not allowed to hold the rows to find the last one
    /// (ADR-0067), so the last *key* is a question for the store.
    pub fn last_record_ord(&self, db: DatabaseId) -> Result<Option<OrderKey>, StorageError> {
        let conn = self.database().conn();
        let max: Option<i64> = conn
            .query_row(
                "SELECT max(ord) FROM db_records WHERE db = ?1",
                params![id(db.as_u64())],
                |r| r.get(0),
            )
            .map_err(sql)?;
        Ok(max.map(|ord| OrderKey(ord_from_db(ord))))
    }

    /// The local month, by the same clock the record stamps use — `NOW`'s
    /// `strftime('now','localtime')`, asked of SQLite itself so "this month"
    /// (the calendar's default) means what a record's `created` stamp prints.
    /// One question, asked once per calendar without a session choice.
    /// `None` when the clock cannot be read, which the caller treats as "no
    /// default" rather than inventing a month.
    pub fn local_month(&self) -> Result<Option<(i32, u32)>, StorageError> {
        let conn = self.database().conn();
        let text: Option<String> = conn
            .query_row("SELECT strftime('%Y-%m','now','localtime')", [], |r| r.get(0))
            .map_err(sql)?;
        let Some(text) = text else {
            return Ok(None);
        };
        let year = text.get(..4).and_then(|s| s.parse::<i32>().ok());
        let month = text.get(5..7).and_then(|s| s.parse::<u32>().ok());
        Ok(match (year, month) {
            (Some(year), Some(month)) if (1..=12).contains(&month) => Some((year, month)),
            _ => None,
        })
    }

    /// The highest id in one of the six database tables, or 0 for an empty one —
    /// what the app's per-session id watermark is seeded from (ADR-0072).
    ///
    /// One question, asked once per table at startup, and a `MAX` over an
    /// integer primary key is the B-tree's own rightmost leaf: it reads no rows.
    /// It is a **watermark and not an allocator** on purpose — the app takes the
    /// value, keeps its own counter, and never asks again, so two creations in
    /// one session cannot race for the same id even though the second one's row
    /// is not in the file yet (the debounced write path is what makes that real:
    /// changes reach SQLite on a timer, not on the keystroke).
    pub fn max_id(&self, table: DbTable) -> Result<u64, StorageError> {
        let conn = self.database().conn();
        let max: Option<i64> = conn
            .query_row(&format!("SELECT max(id) FROM {}", table.name()), [], |r| {
                r.get(0)
            })
            .map_err(sql)?;
        Ok(max.unwrap_or(0).max(0) as u64)
    }

    /// Every value stored for one record, in the stored shape (ADR-0062): what
    /// deleting a row has to capture so that its undo can put the cells back
    /// (ADR-0063's plan).
    ///
    /// One query for the typed rows and one for the list items, rather than a
    /// point read per column: the caller does not know which columns have a
    /// value, and asking per column would be fourteen queries to answer "what is
    /// in this row". The columns are read here as they are stored, and the
    /// *kind* decides which of them is the value — the same rule
    /// [`SqliteRepository::cell`] follows for one cell.
    pub fn record_values(
        &self,
        record: RecordId,
    ) -> Result<Vec<(PropertyId, CellValue)>, StorageError> {
        let conn = self.database().conn();
        let mut stmt = conn
            .prepare(
                "SELECT v.property, p.kind, v.text, v.num, v.flag
                   FROM db_values v
                   JOIN db_properties p ON p.id = v.property
                  WHERE v.record = ?1
                  ORDER BY v.property",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map(params![id(record.as_u64())], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(sql)?;
        let typed: Vec<(i64, String, String, Option<f64>, i64)> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        let mut out = Vec::with_capacity(typed.len());
        for (property, kind, text, num, flag) in typed {
            let property = PropertyId(property as u64);
            let kind = PropertyKind::from_stored(&kind);
            if kind.is_list() {
                // A list cell is its items or nothing: `db_value_items` is the
                // only place its value lives, and an empty list is `Empty` (the
                // store normalises it on the way in, ADR-0062).
                let items = read_cell_items(&conn, record, property)?;
                if !items.is_empty() {
                    out.push((property, CellValue::Items(items)));
                }
                continue;
            }
            if kind.is_computed() || kind.is_derived() {
                // A computed cell stores nothing, and a derived one is the
                // record's own column (ADR-0068, which is why a value row a
                // rogue caller wrote there is not a value any read consults).
                continue;
            }
            let value = match kind.value_kind() {
                ValueKind::Number => num.map(CellValue::Number).unwrap_or(CellValue::Empty),
                ValueKind::Flag => CellValue::Flag(flag != 0),
                _ => CellValue::Text(text),
            };
            if !value.is_empty() {
                out.push((property, value));
            }
        }
        Ok(out)
    }

    /// Which records of one database own a page (ADR-0063), as a map — the one
    /// thing a window read's `RowView` does not carry and the Markdown export
    /// needs (ADR-0065 writes `[title](quire://page/<id>)` for a page-backed
    /// row).
    ///
    /// One query for the whole database and not one per row: the export walks
    /// every row it writes, and a point read per row would be a thousand queries
    /// for a thousand-row file. The predicate is `page IS NOT NULL`, which is a
    /// scan of the rows that *have* a page and not of the table — and with lazy
    /// pages (ADR-0063) that is a small fraction of the rows.
    pub fn record_pages(&self, db: DatabaseId) -> Result<RecordPages, StorageError> {
        let conn = self.database().conn();
        let mut stmt = conn
            .prepare("SELECT id, page FROM db_records WHERE db = ?1 AND page IS NOT NULL")
            .map_err(sql)?;
        let rows = stmt
            .query_map(params![id(db.as_u64())], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(sql)?;
        let mut pages = RecordPages::new();
        for row in rows {
            let (record, page) = row.map_err(sql)?;
            pages.insert(record as u64, PageId(page as u64));
        }
        Ok(pages)
    }
}

/// Which of the six tables an id is being asked about (ADR-0072's watermark).
/// An enum and not a `&str`, because the name is formatted into a statement and
/// the *only* strings that may reach it are the six spelled here — a caller
/// cannot pass `db_records; DROP TABLE pages` if it cannot pass a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbTable {
    Databases,
    Properties,
    Records,
    Views,
}

impl DbTable {
    /// The table's name. `const` so a caller can also print it in a message
    /// without allocating.
    pub const fn name(self) -> &'static str {
        match self {
            DbTable::Databases => "databases",
            DbTable::Properties => "db_properties",
            DbTable::Records => "db_records",
            DbTable::Views => "db_views",
        }
    }
}

/// A record's two instants, or `None` when there is no such record. The free
/// function takes the connection its caller already holds — the method on the
/// repository is this plus the lock — because a store method that reached back
/// through `self` while holding the guard would deadlock on a mutex that is not
/// reentrant.
fn read_record_timestamps(
    conn: &Connection,
    id: RecordId,
) -> Result<Option<RecordTimestamps>, StorageError> {
    conn.query_row(
        "SELECT created, edited FROM db_records WHERE id = ?1",
        params![id.as_u64() as i64],
        |r| {
            Ok(RecordTimestamps {
                created: r.get(0)?,
                edited: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(sql)
}

/// One record's stamp as a cell, for the two derived kinds — ADR-0068's
/// "derived from the record itself and from nothing else". The empty string is
/// a record from before step 17, and paints as an empty cell rather than 1970.
fn derived_cell(
    conn: &Connection,
    record: RecordId,
    kind: PropertyKind,
) -> Result<CellValue, StorageError> {
    let Some(stamps) = read_record_timestamps(conn, record)? else {
        return Ok(CellValue::Empty);
    };
    let stamp = if kind == PropertyKind::CreatedTime {
        stamps.created
    } else {
        stamps.edited
    };
    Ok(if stamp.is_empty() {
        CellValue::Empty
    } else {
        CellValue::Text(stamp)
    })
}

// The statement builders live in `super::database_query` (D4, ADR-0076): the
// filter tree, the sort list and the group are all compiled into SQL text there,
// and this file is the only caller that runs the result. What stays here is the
// *execution* — prepare, run, paint — shared by the three read shapes the
// statements serve: the window's slice, a group's slice, and the export's
// whole-table control read.

/// The statement one row read runs, and its binds — for the tests and the
/// probe, which print (and `EXPLAIN`) the query they measured (D1's evidence
/// table). The read path itself goes through [`row_query_plan`] directly,
/// because it needs both halves at once.
#[cfg(test)]
fn row_query(req: &RowRequest<'_>, window: Option<RowWindow>) -> String {
    row_query_plan(req, window).sql
}

/// The bind values `row_query`'s placeholders take, in the order the plan
/// emitted them: the title property, the visible properties, then — per sort
/// term and per filter clause the view hides — the hidden column's id, the
/// filter's own values, the database, then the window. A `Value` and not an
/// `i64` because a filter's binds are heterogeneous by design: a number clause
/// binds a `REAL`, a text clause a string, an id an integer.
#[cfg(test)]
fn row_binds(req: &RowRequest<'_>, window: Option<RowWindow>) -> Vec<Value> {
    row_query_plan(req, window).binds
}

/// The windowed (or, with `None`, whole-table) row read: build the statement
/// with the view's rules compiled in, run it, paint it.
fn read_rows(
    conn: &Connection,
    req: &RowRequest<'_>,
    window: Option<RowWindow>,
) -> Result<Vec<RowView>, StorageError> {
    run_row_query(conn, row_query_plan(req, window), req)
}

/// Run one row-shaped statement and paint what it came back with
/// (`core::database_property::paint`: an option id becomes a name, a number
/// goes through its format, a file id through the window's attachment names).
/// All three read shapes — the window's slice, a group's slice, the export's
/// whole-table control read — end here, so the painting rules exist once and
/// the only thing that differs between them is the statement.
fn run_row_query(
    conn: &Connection,
    query: super::database_query::RowQuery,
    req: &RowRequest<'_>,
) -> Result<Vec<RowView>, StorageError> {
    let super::database_query::RowQuery { sql: text, binds } = query;
    let mut stmt = conn.prepare(&text).map_err(sql)?;
    let mut raw: Vec<RawRow> = Vec::new();
    {
        let mut rows = stmt
            .query(params_from_iter(binds.iter().cloned()))
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
    let names = read_attachment_names(req, &items, conn)?;

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
            painted.push(column.paint(&value, &names));
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
        // A derived kind's stamp arrives in the text slot (it is a column of
        // the record, not of `db_values`), and step 17's `''` means "not known"
        // — the absence of a value, exactly as `cell` reads it (ADR-0068).
        ValueKind::Derived => match text {
            Some(stamp) if !stamp.is_empty() => CellValue::Text(stamp),
            _ => CellValue::Empty,
        },
    }
}

/// The names of the attachments this window's `files` cells point at (ADR-0029/
/// ADR-0030's store, ADR-0062's "files is the same channel"): one query for the
/// whole window, and only when the view shows a files column.
///
/// The ids come from the rows already in hand, never from the table, so the
/// window bounds this query the way it bounds the row read. An id whose row is
/// gone is simply absent from the answer, and the cell paints the id itself
/// (ADR-0069) — a visible missing file rather than a blank cell.
fn read_attachment_names(
    req: &RowRequest<'_>,
    items: &HashMap<(i64, i64), Vec<String>>,
    conn: &Connection,
) -> Result<HashMap<String, String>, StorageError> {
    let file_columns: Vec<i64> = req
        .columns
        .iter()
        .filter(|c| c.kind == PropertyKind::Files)
        .map(|c| id(c.id.as_u64()))
        .collect();
    if file_columns.is_empty() {
        return Ok(HashMap::new());
    }
    let mut wanted: BTreeSet<i64> = BTreeSet::new();
    for ((_, property), ids) in items {
        if !file_columns.contains(property) {
            continue;
        }
        for item in ids {
            if let Ok(id) = item.parse::<i64>() {
                wanted.insert(id);
            }
        }
    }
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<i64> = wanted.into_iter().collect();
    let query = format!(
        "SELECT id, name FROM attachments WHERE id IN ({})",
        placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&query).map_err(sql)?;
    let rows = stmt
        .query_map(params_from_iter(ids.iter().copied()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(sql)?;
    let mut names = HashMap::new();
    for row in rows {
        let (id, name) = row.map_err(sql)?;
        names.insert(id.to_string(), name);
    }
    Ok(names)
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
        "INSERT INTO databases (id, name, template) VALUES (?1, ?2, ?3)",
        params![id(db.id.as_u64()), db.name, db.template],
    )
    .map_err(sql)?;
    Ok(())
}

/// The template document, replaced whole (ADR-0086). The caller read-edited-
/// wrote the text (`database_template`'s builders); storage stores it and
/// does no JSON — the same split `set_view_definition` and
/// `set_property_config` keep for the other two documents.
pub(crate) fn set_database_template(
    tx: &Transaction,
    db: DatabaseId,
    template: &str,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE databases SET template = ?2 WHERE id = ?1",
            params![id(db.as_u64()), template],
        )
        .map_err(sql)?;
    require_hit(n, "DatabaseTemplateSet", db.as_u64())
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

/// Replace a column's `config` document whole — `Change::PropertyConfigSet`'s
/// write (D6, ADR-0082). The caller read-edit-wrote the document
/// (`core::database_formula::config_set_formula` for the formula key), so this
/// is one `UPDATE` and no JSON happens here: storage stores what the caller
/// decided, which is what keeps the document's shape defined in one place.
pub(crate) fn set_property_config(
    tx: &Transaction,
    property: PropertyId,
    config: &str,
) -> Result<(), StorageError> {
    let n = tx
        .execute(
            "UPDATE db_properties SET config = ?2 WHERE id = ?1",
            params![id(property.as_u64()), config],
        )
        .map_err(sql)?;
    require_hit(n, "PropertyConfigSet", property.as_u64())
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
    // The record's birthday (ADR-0068): stamped by the one statement that
    // creates a record, and never written again — a rename, a cell edit and a
    // drag all leave it where it is. A *redo* of the creation stamps a new
    // moment, which is the honest answer to "when was this row made": it was
    // just made again.
    tx.execute(
        &format!(
            "INSERT INTO db_records (id, db, page, ord, created, edited)
             VALUES (?1, ?2, ?3, ?4, {NOW}, {NOW})"
        ),
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

/// Move a record's `edited` stamp to now (ADR-0068). **Content is the rule**: a
/// cell write is content, and so is a page title (a page-backed record's title
/// *is* `pages.title`, ADR-0063, which is why [`touch_edited_by_page`] exists
/// for the one write that comes in through another module). Moving a row in the
/// listing, or pointing it at a page, is the record's *frame*, and dragging a
/// row is not editing it — so those two leave the stamp alone, which is a
/// decision and not an oversight.
fn touch_record(tx: &Transaction, record: i64) -> Result<(), StorageError> {
    tx.execute(
        &format!("UPDATE db_records SET edited = {NOW} WHERE id = ?1"),
        params![record],
    )
    .map_err(sql)?;
    Ok(())
}

/// The record whose page was just renamed has its `edited` stamp moved
/// (ADR-0068). Called from `repository::apply_one`'s `PageTitleSet` arm, which
/// is the one write path outside this module that changes a record's content.
///
/// No `require_hit`: most pages are not a record's face, and zero rows updated
/// is the ordinary answer rather than a missing row.
pub(crate) fn touch_edited_by_page(tx: &Transaction, page: PageId) -> Result<(), StorageError> {
    tx.execute(
        &format!("UPDATE db_records SET edited = {NOW} WHERE page = ?1"),
        params![id(page.as_u64())],
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
    // A cell is the record's content, so writing one moves `last edited time`
    // (ADR-0068) — including a write that *clears* a cell, because clearing is
    // an edit. One statement more per cell write, and it is the price of the
    // stamp not being a lie; D8's "cost of editing one cell" number includes it.
    touch_record(tx, record)?;
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

/// Raw rows of the six tables, exactly as they were — including the two record
/// timestamps (ADR-0068): a checkpoint, a repair or a LAN pull replaces the
/// document, and a record that survives it keeps the birthday it had, not the
/// moment the file was rewritten.
pub(crate) struct DatabaseSnapshot {
    /// The template column (v20, ADR-0086) rides with the row: a checkpoint or
    /// a LAN pull that dropped it would silently un-template every database —
    /// the exact class of loss ADR-0066 exists for.
    databases: Vec<(i64, String, String)>,
    properties: Vec<(i64, i64, String, String, String, i64)>,
    records: Vec<(i64, i64, Option<i64>, i64, String, String)>,
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
        let mut stmt = conn
            .prepare("SELECT id, name, template FROM databases")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
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
            .prepare("SELECT id, db, page, ord, created, edited FROM db_records")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
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
    for (id, name, template) in &snapshot.databases {
        tx.execute(
            "INSERT OR REPLACE INTO databases (id, name, template) VALUES (?1, ?2, ?3)",
            params![id, name, template],
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
    for (id, db, page, ord, created, edited) in &snapshot.records {
        if page.is_some_and(|p| !kept_pages.contains(&p)) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO db_records (id, db, page, ord, created, edited)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, db, page, ord, created, edited],
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
    use crate::core::database_property::OptionId;
    use crate::core::persistence::{Change, Repository};
    use crate::core::types::{Attachment, AttachmentId, Page, PageFont};

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
        let req = RowRequest::new(db.id, PropertyId(1), &columns);
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
        let req = RowRequest::new(db.id, PropertyId(1), &columns);
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
        let req = RowRequest::new(db.id, PropertyId(1), &columns);
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

    // ─── D2: the property system ───────────────────────────────────────────
    //
    // `core::database_property`'s tests are about the rules; these are about
    // what lands in SQLite and comes back — one round trip per kind, the
    // borders, and the two things this slice exists for: the order of a window
    // is SQL's, and the two derived kinds read the record rather than a value
    // row (ADR-0068/ADR-0069).

    /// A column with settings, for the kinds whose round trip is about them.
    fn with_config(id: u64, kind: PropertyKind, config: &str, ordinal: u64) -> Property {
        Property {
            config: config.into(),
            ..property(id, kind, ordinal)
        }
    }

    /// The status/select list the tests share: one option with a colour, one
    /// without, exactly the document ADR-0061 writes down.
    const OPTIONS: &str =
        r#"{"options":[{"id":7,"name":"Done","color":"green"},{"id":2,"name":"Doing"}]}"#;

    /// Records written through the app's own change path: one `RecordCreated`
    /// and one `CellSet` per (property, value) given. Returns their ids in
    /// listing order.
    fn write_rows(repo: &SqliteRepository, values: &[Vec<(u64, CellValue)>]) -> Vec<RecordId> {
        let mut changes = Vec::new();
        for (index, cells) in values.iter().enumerate() {
            let record = RecordId(1_000 + index as u64);
            changes.push(Change::RecordCreated(Record::bare(
                record,
                DatabaseId(1),
                OrderKey(((index as u64) + 1) << 32),
            )));
            for (property, value) in cells {
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(*property),
                    value: value.clone(),
                });
            }
        }
        repo.apply(&changes).unwrap();
        (0..values.len()).map(|i| RecordId(1_000 + i as u64)).collect()
    }

    /// The first visible cell of every row, in the order a window read returns
    /// them — which is the only order these tests care about.
    fn column_read(
        repo: &SqliteRepository,
        columns: &[Property],
        sort: Option<SortSpec>,
        window: RowWindow,
    ) -> Vec<String> {
        let mut req = RowRequest::new(DatabaseId(1), PropertyId(1), columns);
        req.sorts = sort.as_slice();
        repo.window_rows(&req, window)
            .unwrap()
            .iter()
            .map(|row| row.cells[0].clone())
            .collect()
    }

    /// Every kind §三十九 lists, once: the value is built by the column's own
    /// `parse` (the input path), written as a `CellSet`, read back typed
    /// (`cell`) and painted in a window — so one test covers "stored and read
    /// back is itself" for text-shaped, numeric, flag, list and derived kinds
    /// at once.
    #[test]
    fn a_cell_of_every_kind_round_trips_through_the_store() {
        let repo = store();
        let columns = vec![
            property(2, PropertyKind::Text, 1),
            property(3, PropertyKind::Number, 2),
            with_config(4, PropertyKind::Number, r#"{"format":"percent"}"#, 3),
            with_config(5, PropertyKind::Select, OPTIONS, 4),
            with_config(6, PropertyKind::Status, OPTIONS, 5),
            property(7, PropertyKind::Date, 6),
            with_config(8, PropertyKind::Date, r#"{"format":"datetime"}"#, 7),
            property(9, PropertyKind::Checkbox, 8),
            property(10, PropertyKind::Url, 9),
            property(11, PropertyKind::Email, 10),
            property(12, PropertyKind::Phone, 11),
            with_config(13, PropertyKind::MultiSelect, OPTIONS, 12),
            property(14, PropertyKind::Files, 13),
            property(15, PropertyKind::CreatedTime, 14),
            property(16, PropertyKind::LastEditedTime, 15),
        ];
        // The title column is ADR-0061's one `title` kind: `seed` writes it, so
        // it is not one of the view's columns below — and its painted form is
        // the row's own title rather than a cell.
        let title = Database::new(DatabaseId(1), "Tasks").title_property(PropertyId(1));
        let db = seed(&repo, &columns);
        // The attachment the files cell points at, through the one channel
        // attachments have (ADR-0029/ADR-0030): a files column stores the id
        // and the *name* a user reads comes from the row.
        repo.apply(&[Change::AttachmentAdded(Attachment {
            id: AttachmentId(12),
            name: "report.pdf".into(),
            file: "a1b2.pdf".into(),
            thumb: String::new(),
            mime: "application/pdf".into(),
            bytes: 4_096,
            width: 0,
            height: 0,
        })])
        .unwrap();
        let record = write_rows(&repo, &[vec![]])[0];

        // (property, input, what the store holds, what a cell paints). The
        // painted form is the part D2 owns: an option id becomes a name, a
        // number goes through its format.
        let scalars: Vec<(u64, &str, CellValue, &str)> = vec![
            (1, "Write the brief", CellValue::Text("Write the brief".into()), "Write the brief"),
            (2, "  a note  ", CellValue::Text("  a note  ".into()), "  a note  "),
            (3, "-2.5", CellValue::Number(-2.5), "-2.5"),
            (3, "0", CellValue::Number(0.0), "0"),
            (4, "0.25", CellValue::Number(0.25), "25%"),
            (5, "Done", CellValue::Text("7".into()), "Done"),
            (6, "doing", CellValue::Text("2".into()), "Doing"),
            (7, "2026-09-22T14:03", CellValue::Text("2026-09-22T14:03".into()), "2026-09-22"),
            (8, "2026-09-22T14:03", CellValue::Text("2026-09-22T14:03".into()), "2026-09-22T14:03"),
            (9, "no", CellValue::Flag(false), "No"),
            (9, "yes", CellValue::Flag(true), "Yes"),
            (10, "not a url", CellValue::Text("not a url".into()), "not a url"),
            (11, "a@b", CellValue::Text("a@b".into()), "a@b"),
            (12, "+86 138 0000 0000", CellValue::Text("+86 138 0000 0000".into()), "+86 138 0000 0000"),
        ];
        let table = |property: u64| -> Property {
            if property == 1 {
                return title.clone();
            }
            columns
                .iter()
                .find(|c| c.id == PropertyId(property))
                .unwrap()
                .clone()
        };
        let cell_at = |property: u64| {
            columns
                .iter()
                .position(|c| c.id == PropertyId(property))
                .unwrap()
        };
        for (property, input, stored, painted) in &scalars {
            let column = table(*property);
            let value = match column.parse(input) {
                Ok(value) => value,
                // The two derived kinds take no write at all, and this is the
                // input path refusing rather than the store silently dropping
                // one (ADR-0068).
                Err(e) if column.kind.is_derived() => {
                    assert!(e.message().contains("stores no cell"), "{e}");
                    CellValue::Empty
                }
                Err(e) => panic!("{e}"),
            };
            assert_eq!(&value, stored, "{input:?} parsed into the wrong cell");
            repo.apply(&[Change::CellSet {
                record,
                property: PropertyId(*property),
                value: value.clone(),
            }])
            .unwrap();
            assert_eq!(
                &repo.cell(record, PropertyId(*property)).unwrap(),
                stored,
                "cell {property} did not round trip"
            );
            let req = RowRequest::new(db.id, PropertyId(1), &columns);
            let row = &repo.window_rows(&req, RowWindow { start: 0, end: 1 }).unwrap()[0];
            if *property == 1 {
                assert_eq!(&row.title, painted, "the title column painted wrong");
            } else {
                assert_eq!(&row.cells[cell_at(*property)], painted, "cell {property} painted wrong");
            }
        }

        // The list kinds: multi-select takes option names, files takes
        // attachment ids, and both paint the names a user reads.
        for (property, inputs, stored, painted) in [
            (
                13u64,
                vec!["Done".to_string(), "Doing".to_string()],
                CellValue::Items(vec!["7".into(), "2".into()]),
                "Done, Doing",
            ),
            (
                14,
                vec!["12".to_string()],
                CellValue::Items(vec!["12".into()]),
                "report.pdf",
            ),
        ] {
            let column = table(property);
            let value = column.parse_many(&inputs).unwrap();
            assert_eq!(value, stored);
            repo.apply(&[Change::CellSet {
                record,
                property: PropertyId(property),
                value: value.clone(),
            }])
            .unwrap();
            assert_eq!(repo.cell(record, PropertyId(property)).unwrap(), stored);
            let req = RowRequest::new(db.id, PropertyId(1), &columns);
            let painted_now = repo
                .window_rows(&req, RowWindow { start: 0, end: 1 })
                .unwrap()[0]
                .cells[cell_at(property)]
                .clone();
            assert_eq!(painted_now, painted, "column {property} painted wrong");
        }

        // And the two derived kinds read the record itself (ADR-0068): the
        // stamp the write path gave it, not a value row.
        let stamps = repo.record_timestamps(record).unwrap().unwrap();
        assert!(
            crate::core::database_property::iso_date(&stamps.created).is_some(),
            "{}",
            stamps.created
        );
        assert_eq!(
            repo.cell(record, PropertyId(15)).unwrap(),
            CellValue::Text(stamps.created.clone())
        );
        assert_eq!(
            repo.cell(record, PropertyId(16)).unwrap(),
            CellValue::Text(stamps.edited.clone())
        );
    }

    /// The borders the task asks to be explicit about: a negative, a decimal
    /// and a zero number; an empty string against an absent cell; a very long
    /// text; the three kinds whose invalid forms are **stored anyway**; and the
    /// shapes each kind refuses by name.
    #[test]
    fn the_borders_of_every_type_are_stored_or_refused_by_name() {
        let repo = store();
        let columns = vec![
            property(2, PropertyKind::Text, 1),
            property(3, PropertyKind::Number, 2),
            property(4, PropertyKind::Date, 3),
            property(5, PropertyKind::Url, 4),
            property(6, PropertyKind::Email, 5),
            property(7, PropertyKind::Phone, 6),
            with_config(8, PropertyKind::MultiSelect, OPTIONS, 7),
        ];
        seed(&repo, &columns);
        let record = write_rows(&repo, &[vec![]])[0];

        // A number is a number whatever its sign or size, and zero is a value:
        // "no number" is the absence of a row and never `0` (ADR-0062).
        for (input, stored) in [
            ("-2", -2.0),
            ("-0.5", -0.5),
            ("0", 0.0),
            ("0.0", 0.0),
            ("1e3", 1000.0),
            ("-3.25e-2", -0.0325),
        ] {
            let value = columns[1].parse(input).unwrap();
            assert_eq!(value, CellValue::Number(stored), "{input}");
            repo.apply(&[Change::CellSet {
                record,
                property: PropertyId(3),
                value,
            }])
            .unwrap();
            assert_eq!(
                repo.cell(record, PropertyId(3)).unwrap(),
                CellValue::Number(stored),
                "{input} did not round trip"
            );
            assert!(
                repo.cell(record, PropertyId(3)).unwrap() != CellValue::Empty,
                "a written zero is a row, not an empty cell"
            );
        }
        for bad in ["inf", "-inf", "NaN", "2,5", "two"] {
            assert!(columns[1].parse(bad).is_err(), "{bad} was stored");
        }

        // Clearing: every kind's empty input is the absence of a row.
        for input in ["", "   "] {
            assert_eq!(columns[1].parse(input).unwrap(), CellValue::Empty);
        }
        repo.apply(&[Change::CellSet {
            record,
            property: PropertyId(3),
            value: CellValue::Empty,
        }])
        .unwrap();
        assert_eq!(repo.cell(record, PropertyId(3)).unwrap(), CellValue::Empty);
        assert_eq!(
            raw_count(&repo, "db_values"),
            0,
            "clearing deletes the row rather than blanking it"
        );

        // A text cell with a space in it is content; a very long one is stored
        // whole (the column is TEXT, and no cap here means no silent cut).
        assert_eq!(
            columns[0].parse("   ").unwrap(),
            CellValue::Text("   ".into())
        );
        let long = "長".repeat(50_000);
        let value = columns[0].parse(&long).unwrap();
        repo.apply(&[Change::CellSet {
            record,
            property: PropertyId(2),
            value,
        }])
        .unwrap();
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Text(long.clone()),
            "a 50 000-character cell round trips unchanged"
        );

        // The three string kinds store what was typed even when it is not what
        // it claims to be: a link the user typed is theirs (ADR-0069), and the
        // hint is a hint.
        for (property, typed) in [
            (5u64, "not a url"),
            (5, "HTTP://EXAMPLE.COM"),
            (6, "a@b"),
            (6, "nope"),
            (7, "1234"),
            (7, "call me maybe"),
        ] {
            let value = columns[(property - 2) as usize].parse(typed).unwrap();
            assert_eq!(value, CellValue::Text(typed.into()));
            repo.apply(&[Change::CellSet {
                record,
                property: PropertyId(property),
                value,
            }])
            .unwrap();
            assert_eq!(
                repo.cell(record, PropertyId(property)).unwrap(),
                CellValue::Text(typed.into()),
                "{typed:?} was rewritten or refused"
            );
        }
        use crate::core::database_property::looks_valid;
        assert!(!looks_valid(PropertyKind::Url, "not a url"));
        assert!(looks_valid(PropertyKind::Url, "https://example.com"));
        assert!(!looks_valid(PropertyKind::Email, "a@b"));
        assert!(!looks_valid(PropertyKind::Phone, "call me maybe"));

        // A date's *shape* is the gate (the sort depends on it) and its
        // calendar is not: the padded form is stored, a leap-shaped one is
        // stored, an unpadded one is refused.
        for (input, stored) in [
            ("2026-09-22", "2026-09-22"),
            ("2026-02-30", "2026-02-30"),
            ("2026-09-22T00:00", "2026-09-22T00:00"),
        ] {
            assert_eq!(columns[2].parse(input).unwrap(), CellValue::Text(stored.into()));
        }
        for bad in ["2026-9-2", "22/09/2026", "2026-09-22 14:03", "today"] {
            assert!(columns[2].parse(bad).is_err(), "{bad} was stored as a date");
        }

        // A list keeps duplicates and its order: it is what the user picked,
        // and `db_value_items`' key is (record, property, ord) — not the value,
        // so picking one option twice is two rows and reads back as two.
        let value = columns[6]
            .parse_many(&["Done".to_string(), "Done".to_string()])
            .unwrap();
        assert_eq!(value, CellValue::Items(vec!["7".into(), "7".into()]));
        repo.apply(&[Change::CellSet {
            record,
            property: PropertyId(8),
            value: value.clone(),
        }])
        .unwrap();
        assert_eq!(repo.cell(record, PropertyId(8)).unwrap(), value);
    }

    /// §三十九's easiest silent mistake, told apart in one test: the same two
    /// characters (`2`, `10`) in a number column and in a text column. SQLite
    /// orders the number column by `REAL` and the text column by bytes, and the
    /// two answers are the two orders.
    #[test]
    fn a_number_sorts_as_a_number_and_a_text_column_sorts_as_bytes() {
        let repo = store();
        let number = property(2, PropertyKind::Number, 1);
        let text = property(3, PropertyKind::Text, 2);
        let columns = vec![number.clone(), text.clone()];
        let db = seed(&repo, &columns);
        write_rows(
            &repo,
            &[
                vec![
                    (2, CellValue::Number(2.0)),
                    (3, CellValue::Text("2".into())),
                ],
                vec![
                    (2, CellValue::Number(10.0)),
                    (3, CellValue::Text("10".into())),
                ],
                vec![
                    (2, CellValue::Number(-1.0)),
                    (3, CellValue::Text("-1".into())),
                ],
                vec![(3, CellValue::Text("".into()))],
            ],
        );
        let whole = RowWindow { start: 0, end: 10 };

        assert_eq!(
            column_read(&repo, &columns, None, whole),
            vec!["2", "10", "-1", ""],
            "with no sort the order is the database's own listing"
        );
        // A number column: -1, 2, 10 — the blank last, because "sort by number"
        // does not mean "float the rows with no number to the top" (ADR-0062).
        assert_eq!(
            column_read(&repo, &columns, SortSpec::of(&number, false), whole),
            vec!["-1", "2", "10", ""],
        );
        assert_eq!(
            column_read(&repo, &columns, SortSpec::of(&number, true), whole),
            vec!["10", "2", "-1", ""],
            "and a descending sort leaves the blanks at the bottom too"
        );
        // The same characters as text: 10 before 2, because bytes are not
        // numbers — which is exactly what ADR-0062's typed column exists to
        // avoid, shown rather than asserted.
        assert_eq!(
            column_read(&repo, &columns, SortSpec::of(&text, false), whole),
            vec!["-1", "10", "2", ""],
        );

        // The order really is the statement's: nothing in this crate sorts a
        // `Vec` of rows, and the SQL says which column it compares.
        let mut req = RowRequest::new(db.id, PropertyId(1), &columns);
        let sort = SortSpec::of(&number, false);
        req.sorts = sort.as_slice();
        let query = row_query(&req, Some(whole));
        assert!(
            query.contains("ORDER BY (v0.num IS NULL) ASC, v0.num ASC, r.ord, r.id"),
            "{query}"
        );
        assert!(
            !query.contains("CAST"),
            "a number is never cast text: {query}"
        );
        let sort = SortSpec::of(&text, true);
        req.sorts = sort.as_slice();
        let query = row_query(&req, Some(whole));
        assert!(
            query.contains("ORDER BY (v1.text IS NULL OR v1.text = '') ASC, v1.text DESC, r.ord, r.id"),
            "{query}"
        );
    }

    /// A date's order is time's order *because* the stored shape is fixed
    /// width — so this test writes the shape that breaks it and watches the
    /// order break, which is what makes the parse rule load-bearing rather than
    /// pedantic (ADR-0062/ADR-0069).
    #[test]
    fn a_date_sorts_as_a_time_because_the_stored_shape_is_fixed_width() {
        let repo = store();
        // `datetime`, so the painted cell is the whole stored text: a day-shaped
        // column would print ten bytes of both `2026-09-02` values and the
        // assertion could not tell the two rows apart (ADR-0069's formats).
        let date = with_config(2, PropertyKind::Date, r#"{"format":"datetime"}"#, 1);
        let columns = vec![date.clone()];
        let _db = seed(&repo, &columns);
        write_rows(
            &repo,
            &[
                vec![(2, CellValue::Text("2026-09-10".into()))],
                vec![(2, CellValue::Text("2026-09-02".into()))],
                vec![(2, CellValue::Text("2026-09-02T23:59".into()))],
                vec![(2, CellValue::Text("2026-09-03T00:00".into()))],
                vec![],
            ],
        );
        let whole = RowWindow { start: 0, end: 10 };
        assert_eq!(
            column_read(&repo, &columns, SortSpec::of(&date, false), whole),
            vec!["2026-09-02", "2026-09-02T23:59", "2026-09-03T00:00", "2026-09-10", ""],
            "a string order over the stored bytes is the chronological order"
        );
        assert_eq!(
            column_read(&repo, &columns, SortSpec::of(&date, true), whole),
            vec!["2026-09-10", "2026-09-03T00:00", "2026-09-02T23:59", "2026-09-02", ""],
        );

        // Now the shape the parser refuses, written past it (a build that had
        // no parser, a hand-edited file): `2026-9-2` sorts *after* everything,
        // because "2026-9" is past "2026-1". The rule that refuses it on the
        // way in is the rule that keeps the order right.
        {
            let conn = repo.database().conn();
            conn.execute(
                "UPDATE db_values SET text = '2026-9-2' WHERE record = 1001",
                [],
            )
            .unwrap();
        }
        let sorted = column_read(&repo, &columns, SortSpec::of(&date, false), whole);
        assert_eq!(
            sorted[0], "2026-09-02T23:59",
            "the earliest value left, now that the plain date became an unpadded one"
        );
        assert_eq!(
            sorted[3], "2026-9-2",
            "an unpadded date is not a date: {sorted:?}"
        );
        assert!(
            crate::core::database_property::iso_date("2026-9-2").is_none(),
            "and the input path refuses to store one"
        );

        // A window is a slice of the *order*, not a slice that is then sorted:
        // rows 1..3 of the sorted read are rows 1..3 of that order.
        let all = column_read(&repo, &columns, SortSpec::of(&date, false), whole);
        let middle = column_read(
            &repo,
            &columns,
            SortSpec::of(&date, false),
            RowWindow { start: 1, end: 3 },
        );
        assert_eq!(middle, all[1..3].to_vec());
    }

    /// Sorting by a column the view does not show: one extra join, and the
    /// order is still SQL's (ADR-0064 lets a view document sort by any column).
    #[test]
    fn a_sort_by_a_hidden_column_joins_it_and_orders_in_sql() {
        let repo = store();
        let visible = property(2, PropertyKind::Text, 1);
        let hidden = property(3, PropertyKind::Number, 2);
        let columns = vec![visible.clone()];
        let db = seed(&repo, &[visible, hidden.clone()]);
        write_rows(
            &repo,
            &[
                vec![
                    (2, CellValue::Text("second".into())),
                    (3, CellValue::Number(10.0)),
                ],
                vec![
                    (2, CellValue::Text("first".into())),
                    (3, CellValue::Number(2.0)),
                ],
            ],
        );
        let mut req = RowRequest::new(db.id, PropertyId(1), &columns);
        let sort = SortSpec::of(&hidden, false);
        req.sorts = sort.as_slice();
        let query = row_query(&req, Some(RowWindow { start: 0, end: 10 }));
        assert!(query.contains("LEFT JOIN db_values s0 "), "{query}");
        assert!(
            query.contains("ORDER BY (s0.num IS NULL) ASC, s0.num ASC, r.ord, r.id"),
            "{query}"
        );
        let rows = repo
            .window_rows(&req, RowWindow { start: 0, end: 10 })
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.cells[0].clone()).collect::<Vec<_>>(),
            vec!["first", "second"],
            "the visible column follows the hidden one's order"
        );

        // The plan, which is the evidence the order is the statement's: the
        // join is an index probe on `db_values`' primary key, and the order is
        // a temp B-tree SQLite builds (no index could serve a LEFT JOIN's
        // order for every row). Printed by the probe as well; here it is
        // asserted, because "the sort happens in SQL" is a red line.
        let plan = explain(&repo, &req, RowWindow { start: 0, end: 10 });
        assert!(
            plan.iter().any(|line| line.contains("TEMP B-TREE")),
            "the plan does not show SQL sorting: {plan:?}"
        );
        assert!(
            plan.iter().all(|line| !line.contains("SCAN db_values")),
            "the join is a probe, not a scan: {plan:?}"
        );
    }

    /// One row read's plan, as SQLite explains it — the same call the probe
    /// prints, so a failing assertion carries the plan it is about.
    fn explain(repo: &SqliteRepository, req: &RowRequest<'_>, window: RowWindow) -> Vec<String> {
        let conn = repo.database().conn();
        let query = row_query(req, Some(window));
        let binds = row_binds(req, Some(window));
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap();
        stmt.query_map(rusqlite::params_from_iter(binds.iter().cloned()), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    /// ADR-0068's whole point, in one test: the two time columns are the
    /// record's own, nothing a caller writes can move `created`, and only a
    /// *content* change moves `edited`.
    #[test]
    fn a_derived_column_is_the_records_own_and_never_a_value_row() {
        let repo = store();
        let created = property(2, PropertyKind::CreatedTime, 1);
        let edited = property(3, PropertyKind::LastEditedTime, 2);
        let number = property(4, PropertyKind::Number, 3);
        seed(&repo, &[created.clone(), edited.clone(), number.clone()]);
        let record = write_rows(&repo, &[vec![(4, CellValue::Number(1.0))]])[0];

        // Stamped by the insert, in the stored date shape, and the same instant
        // for both because one statement wrote them.
        let stamps = repo.record_timestamps(record).unwrap().unwrap();
        assert_eq!(stamps.created.len(), 16, "{}", stamps.created);
        assert_eq!(stamps.created, stamps.edited);
        assert!(crate::core::database_property::iso_date(&stamps.created).is_some());

        // Set the pair back, so "moved" and "did not move" are both visible in
        // a test that runs in well under a minute.
        let age = |column: &str| {
            let conn = repo.database().conn();
            conn.execute(
                &format!("UPDATE db_records SET {column} = '2020-01-01T00:00'"),
                [],
            )
            .unwrap();
        };
        let stamps_now = || repo.record_timestamps(record).unwrap().unwrap();
        age("created");
        age("edited");

        // A cell write is content: it moves `edited` and leaves `created`.
        repo.apply(&[Change::CellSet {
            record,
            property: PropertyId(4),
            value: CellValue::Number(2.0),
        }])
        .unwrap();
        assert_eq!(stamps_now().created, "2020-01-01T00:00", "a birthday is a birthday");
        assert_ne!(stamps_now().edited, "2020-01-01T00:00", "editing a cell moved it");

        // Frame, not content: moving a row in the listing and pointing it at a
        // page both leave the stamp alone — dragging a row is not editing it.
        age("edited");
        let page = page(50, "Row");
        repo.apply(&[
            Change::PageCreated(page.clone()),
            Change::RecordOrdSet {
                id: record,
                ord: OrderKey(9 << 32),
            },
            Change::RecordPageSet {
                id: record,
                page: Some(page.id),
            },
        ])
        .unwrap();
        assert_eq!(stamps_now().edited, "2020-01-01T00:00");

        // The record's title now lives in `pages.title` (ADR-0063), and a
        // rename comes in through another module: `repository::apply_one` calls
        // back into this one so the stamp still moves.
        repo.apply(&[Change::PageTitleSet {
            id: page.id,
            title: "Renamed".into(),
        }])
        .unwrap();
        assert_ne!(
            stamps_now().edited,
            "2020-01-01T00:00",
            "the title is this record's content too"
        );
        assert_eq!(stamps_now().created, "2020-01-01T00:00");

        // And the read never consults `db_values` for a derived kind: a rogue
        // writer's row sits there and changes nothing (ADR-0039's discipline,
        // made structural rather than promised).
        age("created");
        age("edited");
        repo.apply(&[Change::CellSet {
            record,
            property: PropertyId(2),
            value: CellValue::Text("1999-12-31T23:59".into()),
        }])
        .unwrap();
        assert_eq!(raw_count(&repo, "db_values"), 2, "the row was written anyway");
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Text("2020-01-01T00:00".into()),
            "the cell still reads the record"
        );
        let columns = vec![created, edited, number];
        let mut req = RowRequest::new(DatabaseId(1), PropertyId(1), &columns);
        let sort = SortSpec::of(&columns[1], true).unwrap();
        req.sorts = std::slice::from_ref(&sort);
        let rows = repo
            .window_rows(&req, RowWindow { start: 0, end: 1 })
            .unwrap();
        assert_ne!(
            rows[0].cells[0], "1999-12-31T23:59",
            "and the window's painted cell does not either"
        );
        assert_eq!(
            rows[0].cells[0],
            crate::core::database_property::paint(
                PropertyKind::CreatedTime,
                "",
                &CellValue::Text("2020-01-01T00:00".into()),
                &()
            )
        );

        // A row from before step 17 (or a hand-made one) has no stamp, and that
        // paints as an empty cell rather than as 1970.
        {
            let conn = repo.database().conn();
            conn.execute("UPDATE db_records SET created = ''", []).unwrap();
        }
        assert_eq!(repo.cell(record, PropertyId(2)).unwrap(), CellValue::Empty);
        // A derived kind refuses every write, clear included (ADR-0068).
        assert!(columns[0].parse("2026-09-22").is_err());
        assert!(columns[0].parse("").is_err());
    }

    /// A `files` cell stores attachment ids (ADR-0062's `db_value_items`) and
    /// paints the names from ADR-0029/ADR-0030's one attachment channel — with
    /// the id itself when the row is gone, because a file whose bytes were
    /// deleted is not an empty cell.
    #[test]
    fn a_files_cell_paints_the_attachment_names_and_an_id_with_no_row_paints_itself() {
        let repo = store();
        let files = property(2, PropertyKind::Files, 1);
        let columns = vec![files.clone()];
        let db = seed(&repo, &columns);
        repo.apply(&[Change::AttachmentAdded(Attachment {
            id: AttachmentId(12),
            name: "report.pdf".into(),
            file: "a1b2.pdf".into(),
            thumb: String::new(),
            mime: "application/pdf".into(),
            bytes: 4_096,
            width: 0,
            height: 0,
        })])
        .unwrap();
        let record = write_rows(&repo, &[vec![]])[0];
        repo.apply(&[Change::CellSet {
            record,
            property: PropertyId(2),
            value: files.parse_many(&["12".to_string()]).unwrap(),
        }])
        .unwrap();

        let req = RowRequest::new(db.id, PropertyId(1), &columns);
        let painted = || {
            repo.window_rows(&req, RowWindow { start: 0, end: 1 }).unwrap()[0].cells[0].clone()
        };
        assert_eq!(painted(), "report.pdf", "the name, not the id");
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Items(vec!["12".into()]),
            "the cell itself is the id"
        );

        // The attachment row goes and the bytes with it (ADR-0030's delete):
        // no foreign key reaches inside `db_value_items`, so the id stays and
        // paints itself — visible, rather than silently blank.
        repo.apply(&[Change::AttachmentDeleted {
            id: AttachmentId(12),
        }])
        .unwrap();
        assert_eq!(painted(), "12");
        assert_eq!(raw_count(&repo, "attachments"), 0);
    }

    /// ADR-0061's rule that makes renaming an option cheap: a value stores an
    /// option's **id**, so an edit of the option list touches no cell. The
    /// list-writing change arm arrives with the option editor (D3/D4); the
    /// *rule* is what this pins, with the document edited where it lives.
    #[test]
    fn an_option_rename_is_one_document_edit_that_touches_no_value() {
        let repo = store();
        let select = with_config(2, PropertyKind::Select, OPTIONS, 1);
        let columns = vec![select.clone()];
        let db = seed(&repo, &columns);
        let record = write_rows(&repo, &[vec![(2, CellValue::Text("7".into()))]])[0];
        let req = RowRequest::new(db.id, PropertyId(1), &columns);
        let painted = || {
            repo.window_rows(&req, RowWindow { start: 0, end: 1 }).unwrap()[0].cells[0].clone()
        };
        assert_eq!(painted(), "Done");

        let mut options = select.options();
        assert!(options.rename(OptionId(7), "Finished"));
        let renamed = options.to_config();
        {
            let conn = repo.database().conn();
            conn.execute(
                "UPDATE db_properties SET config = ?1 WHERE id = 2",
                rusqlite::params![renamed],
            )
            .unwrap();
        }
        let catalog = repo.load_databases().unwrap();
        let column_now = catalog
            .properties_of(DatabaseId(1))
            .find(|p| p.id == PropertyId(2))
            .unwrap()
            .clone();
        assert_eq!(
            column_now.options().get(OptionId(7)).unwrap().name,
            "Finished"
        );
        let req = RowRequest::new(db.id, PropertyId(1), std::slice::from_ref(&column_now));
        let rows = repo
            .window_rows(&req, RowWindow { start: 0, end: 1 })
            .unwrap();
        assert_eq!(rows[0].cells[0], "Finished", "the new name paints");
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Text("7".into()),
            "and the value is byte for byte what it was"
        );
        // An id the list does not have paints itself; so does a column whose
        // document does not parse at all.
        let mut without = column_now.clone();
        without.config = r#"{"options":[{"id":2,"name":"Doing"}]}"#.into();
        let req = RowRequest::new(db.id, PropertyId(1), std::slice::from_ref(&without));
        assert_eq!(
            repo.window_rows(&req, RowWindow { start: 0, end: 1 }).unwrap()[0].cells[0],
            "7"
        );
        without.config = "not json".into();
        let req = RowRequest::new(db.id, PropertyId(1), std::slice::from_ref(&without));
        assert_eq!(
            repo.window_rows(&req, RowWindow { start: 0, end: 1 }).unwrap()[0].cells[0],
            "7"
        );
    }

    /// SPEC §三十九's `person` 降级 (ADR-0071): a name in a text cell, and a
    /// member list that is the values themselves — no table, no accounts, no
    /// ids to reconcile. Written the way a build that knows `person` would have
    /// left the file, because **this** build folds the kind to text at load
    /// (ADR-0061) and so cannot create one.
    #[test]
    fn a_person_column_is_a_name_and_the_member_list_is_derived_from_the_values() {
        let repo = store();
        // `seed` writes the title column itself, so the person column is the
        // only extra one.
        seed(&repo, &[]);
        {
            let conn = repo.database().conn();
            conn.execute(
                "INSERT INTO db_properties (id, db, name, kind, config, ord)
                 VALUES (2, 1, 'Owner', 'person', '', 4352)",
                [],
            )
            .unwrap();
        }
        write_rows(
            &repo,
            &[
                vec![(2, CellValue::Text("Ada".into()))],
                vec![(2, CellValue::Text("Bob".into()))],
                vec![(2, CellValue::Text("Ada".into()))],
                vec![],
            ],
        );

        // The kind folds at load — this build has no account model and no
        // person-specific behaviour to hang on a variant — and the cell is a
        // plain string.
        let catalog = repo.load_databases().unwrap();
        let owner = catalog
            .properties_of(DatabaseId(1))
            .find(|p| p.id == PropertyId(2))
            .unwrap();
        assert_eq!(owner.kind, PropertyKind::Text);
        assert_eq!(
            repo.cell(RecordId(1_000), PropertyId(2)).unwrap(),
            CellValue::Text("Ada".into())
        );

        // The list a picker offers is the distinct names in the file, in a
        // stable order, with no second copy anywhere to go stale.
        assert_eq!(
            repo.workspace_people().unwrap(),
            vec!["Ada".to_string(), "Bob".to_string()]
        );
        // And no member table exists: the names *are* the list.
        {
            let conn = repo.database().conn();
            let tables: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(
                tables.iter().all(|t| !t.contains("person") && !t.contains("member")),
                "{tables:?}"
            );
        }
        // Renaming a person is editing a string — nothing else to reconcile,
        // which is the whole argument for the degradation.
        repo.apply(&[Change::CellSet {
            record: RecordId(1_000),
            property: PropertyId(2),
            value: CellValue::Text("Ada Lovelace".into()),
        }])
        .unwrap();
        assert_eq!(
            repo.workspace_people().unwrap(),
            vec!["Ada".to_string(), "Ada Lovelace".to_string(), "Bob".to_string()],
            "a new name is a new row in the list, not a rename of a member \
             (and \"Ada\" sorts before \"Ada Lovelace\", being its prefix)"
        );
    }

    /// The bulk path (`replace_all`: a checkpoint, a repair, a LAN pull) deletes
    /// the document and puts it back, and it cascades through `db_records.page`.
    /// A record that survives it keeps the birthday it had — the moment the
    /// file was rewritten is not when the row was made (ADR-0066's rule, applied
    /// to ADR-0068's columns).
    #[test]
    fn a_bulk_replace_keeps_the_birthday_of_a_record_it_keeps() {
        let repo = store();
        seed(&repo, &[property(2, PropertyKind::Number, 1)]);
        let record = write_rows(&repo, &[vec![(2, CellValue::Number(1.0))]])[0];
        {
            let conn = repo.database().conn();
            conn.execute(
                "UPDATE db_records SET created = '2020-01-01T00:00', edited = '2020-01-01T00:00'",
                [],
            )
            .unwrap();
        }
        // Every borrow of the connection is scoped: the repository's mutex is
        // not reentrant, so a guard held across a `repo.*` call would deadlock
        // this test rather than fail it.
        let snapshot = {
            let conn = repo.database().conn();
            snapshot_tables(&conn).unwrap()
        };
        // What `replace_all` does to the layer: `DELETE FROM pages` cascades
        // through `db_records.page`, and here the tables are emptied outright.
        {
            let conn = repo.database().conn();
            conn.execute("DELETE FROM db_records", []).unwrap();
        }
        assert!(repo.record_timestamps(record).unwrap().is_none());

        {
            let conn = repo.database().conn();
            let tx = conn.unchecked_transaction().unwrap();
            restore_tables(&tx, &snapshot, &BTreeSet::new()).unwrap();
            tx.commit().unwrap();
        }

        let stamps = repo.record_timestamps(record).unwrap().unwrap();
        assert_eq!(stamps.created, "2020-01-01T00:00");
        assert_eq!(stamps.edited, "2020-01-01T00:00");
        assert_eq!(
            repo.cell(record, PropertyId(2)).unwrap(),
            CellValue::Number(1.0),
            "and the cells came back with it"
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

        let req = RowRequest::new(db.id, title.id, &columns);
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
                .query_map(params_from_iter(binds.iter().cloned()), |r| {
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

    /// D2's number: what a **sorted** window costs on the SQL side, against the
    /// arm §三十九 forbids a view from taking — every row, sorted in Rust.
    ///
    /// The same 10 000 rows as D1's probe, with one number column whose values
    /// are scattered (`(i * 7919) mod 10007` is a permutation over pairs), so
    /// neither the insertion order nor the value order is the answer. Both
    /// orders are checked against each other before anything is timed: a fast
    /// wrong order would be worse than a slow right one.
    #[test]
    #[ignore = "prints a measurement; run with --release --lib -- --ignored --nocapture"]
    fn a_sorted_window_costs_its_window_and_sorting_in_memory_costs_the_table() {
        let dir = crate::testing::ScratchDir::new("db-sorted-window");
        let path = dir.join("quire.db");
        let repo = SqliteRepository::open(&path).unwrap();

        let db = Database::new(DatabaseId(1), "Tasks");
        let title = db.title_property(PropertyId(1));
        let number = property(2, PropertyKind::Number, 1);
        let date = property(3, PropertyKind::Date, 2);
        let columns = vec![number.clone(), date.clone()];
        let mut schema = vec![
            Change::DatabaseCreated(db.clone()),
            Change::PropertyAdded(title.clone()),
            Change::ViewAdded(db.first_view(ViewId(1))),
        ];
        schema.extend(columns.iter().cloned().map(Change::PropertyAdded));
        repo.apply(&schema).unwrap();

        let started = Instant::now();
        for batch in 0..(ROWS / BATCH) {
            let mut changes = Vec::with_capacity(BATCH * 3);
            for i in 0..BATCH {
                let index = batch * BATCH + i;
                let record = RecordId(index as u64 + 1);
                changes.push(Change::RecordCreated(Record::bare(
                    record,
                    db.id,
                    OrderKey(((index as u64) + 1) << 32),
                )));
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(2),
                    value: CellValue::Number(((index * 7919) % 10_007) as f64),
                });
                changes.push(Change::CellSet {
                    record,
                    property: PropertyId(3),
                    value: CellValue::Text(format!(
                        "2026-{:02}-{:02}T{:02}:{:02}",
                        1 + (index / 28) % 12,
                        1 + index % 28,
                        index % 24,
                        index % 60
                    )),
                });
            }
            repo.apply(&changes).unwrap();
        }
        let insert_ms = started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(repo.record_count(db.id).unwrap(), ROWS);

        let sort = SortSpec::of(&number, false).unwrap();
        let sorted = RowRequest {
            db: db.id,
            title: title.id,
            columns: &columns,
            sorts: std::slice::from_ref(&sort),
            filter: None,
            search: None,
        };
        let plain = RowRequest {
            sorts: &[],
            filter: None,
            ..sorted
        };
        let geometry = ViewGeometry::new(32.0, 720.0);
        let top = crate::core::database::window(ROWS, geometry, 0.0);
        let bottom = crate::core::database::window(
            ROWS,
            geometry,
            crate::core::database::max_scroll_y(ROWS, geometry),
        );

        // Warm the page cache and the statement machinery.
        assert_eq!(repo.window_rows(&sorted, RowWindow { start: 0, end: 1 }).unwrap().len(), 1);

        let timed = |what: &str| {
            let started = Instant::now();
            let rows = match what {
                "sorted" => repo.window_rows(&sorted, top),
                "sorted-bottom" => repo.window_rows(&sorted, bottom),
                "sorted-all" => repo.window_rows(&sorted, RowWindow { start: 0, end: ROWS }),
                _ => repo.window_rows(&plain, bottom),
            }
            .unwrap();
            (started.elapsed().as_secs_f64() * 1e6, rows)
        };

        // The SQL side, four readings: the window a view actually asks for at
        // the top and at the bottom, the same sort without a window (what the
        // order itself costs), and D1's unsorted bottom window for a
        // same-session comparison — the sort's price is the difference.
        let (top_us, top_rows) = timed("sorted");
        let (bottom_us, bottom_rows) = timed("sorted-bottom");
        let (all_sorted_us, all_sorted) = timed("sorted-all");
        let (bottom_plain_us, _) = timed("plain-bottom");
        assert_eq!(top_rows.len(), top.len());
        assert_eq!(bottom_rows.len(), bottom.len());
        assert_eq!(all_sorted.len(), ROWS);

        // The control arm: every row, then a numeric sort in Rust — which is
        // what "filter and sort in the UI" would mean, and what §三十九's red
        // line is about.
        let _ = repo.unwindowed_rows(&plain).unwrap();
        let started = Instant::now();
        let (all_rows, all_bytes) = measure(|| repo.unwindowed_rows(&plain).unwrap());
        let all_rows_ms = started.elapsed().as_secs_f64() * 1e3;
        let started = Instant::now();
        let mut values: Vec<f64> = all_rows
            .iter()
            .map(|row| row.cells[0].parse::<f64>().unwrap_or(f64::NAN))
            .collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let rust_sort_us = started.elapsed().as_secs_f64() * 1e6;
        let (window_rows, window_bytes) = measure(|| repo.window_rows(&sorted, top).unwrap());
        assert_eq!(window_rows.len(), top.len());

        // Both arms agree on the first window, so the fast one is not fast
        // because it is wrong.
        let sql_first: Vec<f64> = top_rows
            .iter()
            .map(|row| row.cells[0].parse::<f64>().unwrap_or(f64::NAN))
            .collect();
        assert_eq!(sql_first, values[..top.len()].to_vec());

        // And the date column, ordered the way the stored shape promises.
        let date_sort = SortSpec::of(&date, true).unwrap();
        let by_date = RowRequest {
            db: db.id,
            title: title.id,
            columns: &columns,
            sorts: std::slice::from_ref(&date_sort),
            filter: None,
            search: None,
        };
        let started = Instant::now();
        let date_rows = repo.window_rows(&by_date, top).unwrap();
        let date_top_us = started.elapsed().as_secs_f64() * 1e6;
        let date_first: Vec<String> = date_rows.iter().map(|r| r.cells[1].clone()).collect();
        assert!(date_first.windows(2).all(|w| w[0] >= w[1]), "descending: {date_first:?}");

        // What one cell write costs *now* that ADR-0068 moves `edited` with it:
        // 300 single-cell transactions, so D6's "cost of editing one cell" has
        // its first reading and the bump's price is inside it.
        let started = Instant::now();
        for i in 0..300u64 {
            repo.apply(&[Change::CellSet {
                record: RecordId(i % 100 + 1),
                property: PropertyId(2),
                value: CellValue::Number(i as f64),
            }])
            .unwrap();
        }
        let cell_write_us = started.elapsed().as_secs_f64() * 1e6 / 300.0;

        // The same writes with the commit taken out of them: one transaction
        // around all 10 000, so the per-statement cost (the `db_values` upsert
        // *and* ADR-0068's `edited` bump) is separable from what one transaction
        // costs on this machine. The difference between the two readings is the
        // fsync; the statement pair is what D6's one-cell number is about.
        let started = Instant::now();
        {
            let conn = repo.database().conn();
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..ROWS as u64 {
                set_cell(
                    &tx,
                    RecordId(i % 1_000 + 1),
                    PropertyId(2),
                    &CellValue::Number(i as f64),
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let batched_cell_us = started.elapsed().as_secs_f64() * 1e6 / ROWS as f64;

        // The evidence: the statement, its binds, and the plan SQLite chose.
        let (query, binds, plan) = {
            let conn = repo.database().conn();
            let query = row_query(&sorted, Some(top));
            let binds = row_binds(&sorted, Some(top));
            let plan: Vec<String> = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap()
                .query_map(params_from_iter(binds.iter().cloned()), |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            (query, binds, plan)
        };

        let rows_mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        let control_ms = all_rows_ms + rust_sort_us / 1000.0;
        println!("database sort probe: {ROWS} records, one number column and one date column");
        println!(
            "  insert: {ROWS} records + {} cells in {BATCH}-change batches: {insert_ms:.1} ms",
            ROWS * 3
        );
        println!(
            "  sql, number ascending: top window {} rows in {top_us:.1} us (LIMIT {} OFFSET {})",
            top_rows.len(),
            top.len(),
            top.start
        );
        println!(
            "  sql, number ascending: bottom window {} rows in {bottom_us:.1} us                          (LIMIT {} OFFSET {})",
            bottom_rows.len(),
            bottom.len(),
            bottom.start
        );
        println!(
            "  sql, the same sort with no window: all {ROWS} rows in {:.1} ms — the order              itself, without the slice",
            all_sorted_us / 1000.0
        );
        println!(
            "  sql, the same bottom window with no sort: {bottom_plain_us:.1} us                           (so the sort adds {:.1} ms to a scroll)",
            (bottom_us - bottom_plain_us) / 1000.0
        );
        println!(
            "  sql, date descending: top window in {date_top_us:.1} us"
        );
        println!(
            "  control (fetch everything, sort in Rust): {:.1} ms all rows + {:.3} ms sort              = {control_ms:.1} ms, {all_bytes} B ({:.2} MB) held",
            all_rows_ms,
            rust_sort_us / 1000.0,
            rows_mb(all_bytes)
        );
        println!(
            "  the window against the control: {:.0}x in time, {:.0}x in bytes",
            control_ms * 1000.0 / top_us.max(1.0),
            all_bytes as f64 / window_bytes.max(1) as f64
        );
        println!("  one cell write: {cell_write_us:.1} us with its own transaction, {batched_cell_us:.1} us batched");
        println!("  query: {query}");
        println!("  binds (?1 = title, then the columns, the database, the window): {binds:?}");
        println!("  plan:");
        for line in &plan {
            println!("    {line}");
        }
        println!(
            "{{\"label\":\"track3-d2-sort\",\"date\":\"2026-09-22\",\
             \"harness\":\"cargo test --release --lib -- --ignored --nocapture\",\
             \"records\":{ROWS},\"insert_ms\":{insert_ms:.1},\"sorted_top_rows\":{},\
             \"sorted_top_us\":{top_us:.1},\"sorted_bottom_rows\":{},\"sorted_bottom_us\":{bottom_us:.1},\
             \"sorted_all_rows\":{},\"sorted_all_ms\":{:.1},\"unsorted_bottom_us\":{bottom_plain_us:.1},\
             \"date_top_us\":{date_top_us:.1},\"control_all_rows\":{},\"control_all_ms\":{all_rows_ms:.1},\
             \"control_sort_us\":{rust_sort_us:.1},\"control_total_ms\":{control_ms:.1},\
             \"heap_all_rows_bytes\":{all_bytes},\"heap_window_bytes\":{window_bytes},\
             \"cell_write_us\":{cell_write_us:.1},\"batched_cell_us\":{batched_cell_us:.1}}}",
            top_rows.len(),
            bottom_rows.len(),
            all_sorted.len(),
            all_sorted_us / 1000.0,
            all_rows.len(),
        );
    }
}
