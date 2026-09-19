// SQLite implementation of the `core::persistence::Repository` contract
// (ADR-0012). One call, one transaction: `apply` and `replace_all` are
// atomic — an intermediate state is never observable (SPEC §十八).

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{params, Connection, Transaction};

use crate::core::persistence::{Change, Repository, StorageError};
use crate::core::types::{Block, BlockId, BlockKind, Mark, MarkKind, OrderKey, Page, PageId, PersistedState};

use super::backup::{self, OpenReport};
use super::database::{ord_from_db, ord_to_db, Database};
use super::search_index::{self, Match, SearchRequest};

pub struct SqliteRepository {
    db: Database,
}

fn sql(e: rusqlite::Error) -> StorageError {
    StorageError::Sql(e.to_string())
}

fn bool_to_db(value: bool) -> i64 {
    i64::from(value)
}

fn db_to_bool(value: i64) -> bool {
    value != 0
}

impl SqliteRepository {
    /// Open (or create) the database at `path`, migrate, and run the
    /// startup integrity check before returning (SPEC §二十五). A main file
    /// that fails that check is restored from the newest readable `.bak<N>`
    /// snapshot, and every successful open rotates the snapshot family
    /// (ADR-0015).
    ///
    /// Use [`Self::open_with_report`] when the caller needs to say out loud
    /// that data rolled back or that no snapshot could be written.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        Ok(Self::open_with_report(path)?.0)
    }

    /// `open`, plus the recovery facts of this particular startup.
    pub fn open_with_report(path: &Path) -> Result<(Self, OpenReport), StorageError> {
        let (db, report) = backup::open_with_recovery(path)?;
        Ok((SqliteRepository { db }, report))
    }

    /// Disposable repository for tests; same schema, no journal.
    pub fn in_memory() -> Result<Self, StorageError> {
        Ok(SqliteRepository {
            db: Database::open_in_memory()?,
        })
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Ranked full-text matches (SPEC §二十). Deliberately *not* on the
    /// `Repository` trait: search is an implementation capability of the
    /// SQLite backend, and the change contract stays untouched (ADR-0014).
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<Match>, StorageError> {
        search_index::matches(&self.db.conn(), req)
    }
}

impl Repository for SqliteRepository {
    fn load(&self) -> Result<PersistedState, StorageError> {
        let conn = self.db.conn();

        let mut pages = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, title, parent, ord, favorite, expanded FROM pages")
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(sql)?;
            for row in rows {
                let (id, title, parent, ord, favorite, expanded) = row.map_err(sql)?;
                pages.push(Page {
                    id: PageId(id as u64),
                    title,
                    parent: parent.map(|p| PageId(p as u64)),
                    order: OrderKey(ord_from_db(ord)),
                    favorite: db_to_bool(favorite),
                    expanded: db_to_bool(expanded),
                });
            }
        }

        let mut blocks = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT b.id, b.page, bc.parent, bc.ord, b.kind, b.text, b.checked
                     FROM blocks b
                     JOIN block_children bc ON bc.block = b.id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                })
                .map_err(sql)?;
            for row in rows {
                let (id, page, parent, ord, kind, text, checked) = row.map_err(sql)?;
                let Some(kind) = BlockKind::try_from_str(&kind) else {
                    // Our own writes always emit `as_str()`; an unknown
                    // string means the file was tampered with or truncated.
                    return Err(StorageError::Corrupt(format!(
                        "block {id} has unknown kind {kind:?}"
                    )));
                };
                blocks.push(Block {
                    id: BlockId(id as u64),
                    page: PageId(page as u64),
                    parent: parent.map(|p| BlockId(p as u64)),
                    order: OrderKey(ord_from_db(ord)),
                    kind,
                    text,
                    checked: db_to_bool(checked),
                    marks: Vec::new(),
                });
            }
        }

        // inline marks (M6), grouped per block
        {
            let mut stmt = conn
                .prepare("SELECT block, start, end, kind, url FROM marks ORDER BY block, start")
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(sql)?;
            let mut marks_by_block: std::collections::HashMap<i64, Vec<Mark>> =
                std::collections::HashMap::new();
            for row in rows {
                let (block, start, end, kind, url) = row.map_err(sql)?;
                let Some(kind) = MarkKind::try_from_str(&kind) else {
                    return Err(StorageError::Corrupt(format!(
                        "mark on block {block} has unknown kind {kind:?}"
                    )));
                };
                marks_by_block.entry(block).or_default().push(Mark {
                    start: start as usize,
                    end: end as usize,
                    kind,
                    url,
                });
            }
            for b in &mut blocks {
                if let Some(marks) = marks_by_block.remove(&(b.id.as_u64() as i64)) {
                    b.marks = marks;
                }
            }
        }

        let mut meta = BTreeMap::new();
        read_map(&conn, "metadata", &mut meta)?;
        let mut settings = BTreeMap::new();
        read_map(&conn, "settings", &mut settings)?;

        Ok(PersistedState {
            pages,
            blocks,
            meta,
            settings,
        })
    }

    fn apply(&self, changes: &[Change]) -> Result<(), StorageError> {
        if changes.is_empty() {
            return Ok(());
        }
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql)?;
        for change in changes {
            apply_one(&tx, change)?;
        }
        // Deletes cascade through FKs without telling the index, so one
        // orphan sweep per batch that removed anything.
        if changes.iter().any(is_delete) {
            search_index::prune(&tx)?;
        }
        tx.commit().map_err(sql)?;
        Ok(())
    }

    fn replace_all(&self, state: &PersistedState) -> Result<(), StorageError> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql)?;
        // Bulk insert order in `state` is unspecified (parents may arrive
        // after children), so defer FK enforcement to commit; the commit
        // still rejects any dangling reference or cycle atomically.
        tx.pragma_update(None, "defer_foreign_keys", "ON")
            .map_err(sql)?;
        tx.execute("DELETE FROM blocks", []).map_err(sql)?; // cascades block_children
        tx.execute("DELETE FROM pages", []).map_err(sql)?; // cascades child pages + blocks
        tx.execute("DELETE FROM metadata", []).map_err(sql)?;
        tx.execute("DELETE FROM settings", []).map_err(sql)?;
        // FTS5 tables have no FKs, so the mirror is cleared by hand; the
        // inserts below re-index every row.
        tx.execute("DELETE FROM search_blocks", []).map_err(sql)?;
        tx.execute("DELETE FROM search_pages", []).map_err(sql)?;
        // A→B→A parent references satisfy FK rules but would spin the
        // sidebar tree forever, so the bulk path validates acyclicity.
        detect_cycle(
            state
                .pages
                .iter()
                .map(|p| (p.id, p.parent))
                .collect::<BTreeMap<_, _>>(),
            "page",
        )?;
        detect_cycle(
            state
                .blocks
                .iter()
                .map(|b| (b.id, b.parent))
                .collect::<BTreeMap<_, _>>(),
            "block",
        )?;
        for page in &state.pages {
            insert_page(&tx, page)?;
        }
        for block in &state.blocks {
            insert_block(&tx, block)?;
        }
        write_map(&tx, "metadata", &state.meta)?;
        write_map(&tx, "settings", &state.settings)?;
        tx.commit().map_err(sql)?;
        Ok(())
    }
}

/// Linear walk over the parent chains; any node revisited inside one walk
/// is a cycle. `safe` memoizes verified chains so the total cost is O(n).
fn detect_cycle<K>(parents: BTreeMap<K, Option<K>>, what: &str) -> Result<(), StorageError>
where
    K: Copy + Ord + core::fmt::Debug,
{
    let mut safe: Vec<K> = Vec::new();
    for start in parents.keys() {
        let mut path: Vec<K> = Vec::new();
        let mut cur = Some(*start);
        while let Some(node) = cur {
            if safe.binary_search(&node).is_ok() {
                break;
            }
            if path.contains(&node) {
                return Err(StorageError::Sql(format!(
                    "replace_all: {what} parent cycle through {node:?}"
                )));
            }
            path.push(node);
            cur = parents.get(&node).copied().flatten();
        }
        safe.extend(path);
        safe.sort_unstable();
        safe.dedup();
    }
    Ok(())
}

fn read_map(
    conn: &Connection,
    table: &str,
    into: &mut BTreeMap<String, String>,
) -> Result<(), StorageError> {
    let mut stmt = conn
        .prepare(&format!("SELECT key, value FROM {table}"))
        .map_err(sql)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(sql)?;
    for row in rows {
        let (k, v) = row.map_err(sql)?;
        into.insert(k, v);
    }
    Ok(())
}

fn write_map(
    tx: &Transaction,
    table: &str,
    map: &BTreeMap<String, String>,
) -> Result<(), StorageError> {
    for (k, v) in map {
        tx.execute(
            &format!("INSERT INTO {table} (key, value) VALUES (?1, ?2)"),
            params![k, v],
        )
        .map_err(sql)?;
    }
    Ok(())
}

fn insert_page(tx: &Transaction, page: &Page) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO pages (id, title, parent, ord, favorite, expanded)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            page.id.as_u64() as i64,
            page.title,
            page.parent.map(|p| p.as_u64() as i64),
            ord_to_db(page.order.0),
            bool_to_db(page.favorite),
            bool_to_db(page.expanded),
        ],
    )
    .map_err(sql)?;
    search_index::index_page_title(tx, page.id, &page.title)
}

fn insert_block(tx: &Transaction, block: &Block) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO blocks (id, page, kind, text, checked) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            block.id.as_u64() as i64,
            block.page.as_u64() as i64,
            block.kind.as_str(),
            block.text,
            bool_to_db(block.checked),
        ],
    )
    .map_err(sql)?;
    tx.execute(
        "INSERT INTO block_children (block, parent, ord) VALUES (?1, ?2, ?3)",
        params![
            block.id.as_u64() as i64,
            block.parent.map(|p| p.as_u64() as i64),
            ord_to_db(block.order.0),
        ],
    )
    .map_err(sql)?;
    for m in &block.marks {
        tx.execute(
            "INSERT INTO marks (block, start, end, kind, url) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                block.id.as_u64() as i64,
                m.start as i64,
                m.end as i64,
                m.kind.as_str(),
                m.url,
            ],
        )
        .map_err(sql)?;
    }
    search_index::upsert_block(tx, block.id, &block.text)
}

/// Fail loudly when a mutation targets an id that is not there: silently
/// dropping a change would desynchronize the in-memory truth.
fn require_hit(affected: usize, what: &str, id: u64) -> Result<(), StorageError> {
    if affected == 1 {
        Ok(())
    } else {
        Err(StorageError::Sql(format!("{what}: no row with id {id}")))
    }
}

fn is_delete(change: &Change) -> bool {
    matches!(
        change,
        Change::PageDeleted { .. } | Change::BlockDeleted { .. }
    )
}

fn apply_one(tx: &Transaction, change: &Change) -> Result<(), StorageError> {
    match change {
        Change::PageCreated(page) => insert_page(tx, page),
        Change::PageTitleSet { id, title } => {
            let n = tx
                .execute(
                    "UPDATE pages SET title = ?2 WHERE id = ?1",
                    params![id.as_u64() as i64, title],
                )
                .map_err(sql)?;
            require_hit(n, "PageTitleSet", id.as_u64())?;
            search_index::index_page_title(tx, *id, title)
        }
        Change::PageMoved { id, parent, order } => {
            let n = tx
                .execute(
                    "UPDATE pages SET parent = ?2, ord = ?3 WHERE id = ?1",
                    params![
                        id.as_u64() as i64,
                        parent.map(|p| p.as_u64() as i64),
                        ord_to_db(order.0)
                    ],
                )
                .map_err(sql)?;
            require_hit(n, "PageMoved", id.as_u64())
        }
        Change::PageFavoriteSet { id, favorite } => {
            let n = tx
                .execute(
                    "UPDATE pages SET favorite = ?2 WHERE id = ?1",
                    params![id.as_u64() as i64, bool_to_db(*favorite)],
                )
                .map_err(sql)?;
            require_hit(n, "PageFavoriteSet", id.as_u64())
        }
        Change::PageExpandedSet { id, expanded } => {
            let n = tx
                .execute(
                    "UPDATE pages SET expanded = ?2 WHERE id = ?1",
                    params![id.as_u64() as i64, bool_to_db(*expanded)],
                )
                .map_err(sql)?;
            require_hit(n, "PageExpandedSet", id.as_u64())
        }
        Change::PageDeleted { id } => {
            // Recursive subtree delete; blocks and links cascade via FK.
            let n = tx
                .execute(
                    "WITH RECURSIVE subtree(id) AS (
                        SELECT ?1
                        UNION ALL
                        SELECT p.id FROM pages p JOIN subtree s ON p.parent = s.id
                     )
                     DELETE FROM pages WHERE id IN (SELECT id FROM subtree)",
                    params![id.as_u64() as i64],
                )
                .map_err(sql)?;
            if n == 0 {
                return Err(StorageError::Sql(format!("PageDeleted: no row with id {}", id.as_u64())));
            }
            Ok(())
        }

        Change::BlockInserted(block) => insert_block(tx, block),
        Change::BlockMarksSet { id, marks } => {
            tx.execute("DELETE FROM marks WHERE block = ?1", params![id.as_u64() as i64])
                .map_err(sql)?;
            let block = Block {
                id: *id,
                page: PageId(0),
                parent: None,
                order: OrderKey(0),
                kind: crate::core::BlockKind::Paragraph,
                text: String::new(),
                checked: false,
                marks: marks.clone(),
            };
            for m in &block.marks {
                tx.execute(
                    "INSERT INTO marks (block, start, end, kind, url) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        block.id.as_u64() as i64,
                        m.start as i64,
                        m.end as i64,
                        m.kind.as_str(),
                        m.url,
                    ],
                )
                .map_err(sql)?;
            }
            Ok(())
        }
        Change::BlockTextSet { id, text } => {
            let n = tx
                .execute(
                    "UPDATE blocks SET text = ?2 WHERE id = ?1",
                    params![id.as_u64() as i64, text],
                )
                .map_err(sql)?;
            require_hit(n, "BlockTextSet", id.as_u64())?;
            search_index::upsert_block(tx, *id, text)
        }
        Change::BlockKindSet { id, kind } => {
            let n = tx
                .execute(
                    "UPDATE blocks SET kind = ?2 WHERE id = ?1",
                    params![id.as_u64() as i64, kind.as_str()],
                )
                .map_err(sql)?;
            require_hit(n, "BlockKindSet", id.as_u64())
        }
        Change::BlockCheckedSet { id, checked } => {
            let n = tx
                .execute(
                    "UPDATE blocks SET checked = ?2 WHERE id = ?1",
                    params![id.as_u64() as i64, bool_to_db(*checked)],
                )
                .map_err(sql)?;
            require_hit(n, "BlockCheckedSet", id.as_u64())
        }
        Change::BlockMoved { id, parent, order } => {
            let n = tx
                .execute(
                    "UPDATE block_children SET parent = ?2, ord = ?3 WHERE block = ?1",
                    params![
                        id.as_u64() as i64,
                        parent.map(|p| p.as_u64() as i64),
                        ord_to_db(order.0)
                    ],
                )
                .map_err(sql)?;
            require_hit(n, "BlockMoved", id.as_u64())
        }
        Change::BlockDeleted { id } => {
            let n = tx
                .execute(
                    "WITH RECURSIVE subtree(id) AS (
                        SELECT ?1
                        UNION ALL
                        SELECT bc.block FROM block_children bc JOIN subtree s ON bc.parent = s.id
                     )
                     DELETE FROM blocks WHERE id IN (SELECT id FROM subtree)",
                    params![id.as_u64() as i64],
                )
                .map_err(sql)?;
            if n == 0 {
                return Err(StorageError::Sql(format!("BlockDeleted: no row with id {}", id.as_u64())));
            }
            Ok(())
        }

        Change::MetaSet { key, value } => {
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(sql)?;
            Ok(())
        }
        Change::SettingSet { key, value } => {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(sql)?;
            Ok(())
        }
    }
}
