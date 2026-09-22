// The reference lookup behind the backlink panel (SPEC §四十, ADR-0051).
//
// The panel asks one question — *which blocks point at this page* — and it asks
// it with the page open, so the answer has to be an index seek rather than a
// scan of `marks` or of `blocks`. Migration 16 adds the two indexes that make
// it one (`marks(kind, url)` and `blocks(page_ref)`); SQLite maintains them,
// so this file is only the query side. There is no write path to keep in step
// and no rebuild step that can be forgotten, which is the whole reason the
// reference layer was given indexes instead of a derived table.
//
// **This is a projection, not stored data.** Nothing here writes: the panel is
// derived every time the page is projected (ADR-0039's rule for the table of
// contents, one kind of derived data over), and the two facts it reads — the
// mention's address and a block's `page_ref` — are the same facts the editor
// already renders. A reference cannot go stale, because there is no copy.

use rusqlite::{params, Connection};

use crate::core::persistence::StorageError;
use crate::core::types::{BlockId, PageId};

fn sql(e: rusqlite::Error) -> StorageError {
    StorageError::Sql(e.to_string())
}

/// One block that points at the page being opened.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    /// The referring block — what a click jumps to (`quire://block/<id>`).
    pub block: BlockId,
    /// The page the block lives on, so the list can group by it and the caller
    /// can label it without a second query.
    pub page: PageId,
    /// The referring block's own text, as stored: what the panel shows under
    /// the page's name. Empty for a block whose meaning is not its text (a
    /// picture, a divider) — the panel then shows just the page.
    pub text: String,
    /// True when this came from a mention *inside* the block rather than from a
    /// block that *is* a page reference (`Page` / `Link to page`, ADR-0026).
    /// The panel marks the second kind, because a page that names another page
    /// still counts as referencing it and a reader wants to know which.
    pub block_level: bool,
}

/// Every block whose text mentions `page`, plus every block that *is* a
/// reference to it, best-effort ordered by the page each lives on.
///
/// `limit` caps the rows the panel can draw (it lists a folded window and a
/// count, so a page referenced 200 times costs 200 rows of `count(*)` and not
/// 200 rows of text). The caller passes `count` separately when it wants the
/// real total.
pub fn references_of(
    conn: &Connection,
    page: PageId,
    limit: usize,
) -> Result<Vec<Reference>, StorageError> {
    let uri = crate::core::page_uri(page);
    let mut stmt = conn
        .prepare(
            // The two sources in one pass. A mention is a mark whose payload is
            // this page's address — `kind` first because that is the index's
            // first column, which is what keeps this an index seek. A
            // block-level reference is `blocks.page_ref`, its own index.
            "SELECT b.id, b.page, b.text, 0 AS block_level
               FROM marks m JOIN blocks b ON b.id = m.block
              WHERE m.kind = 'mention' AND m.url = ?1
             UNION
             SELECT b.id, b.page, b.text, 1
               FROM blocks b
              WHERE b.page_ref = ?2
             ORDER BY 2, 1",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map(params![uri, page.as_u64() as i64], |r| {
            Ok(Reference {
                block: BlockId(r.get::<_, i64>(0)? as u64),
                page: PageId(r.get::<_, i64>(1)? as u64),
                text: r.get::<_, String>(2)?,
                block_level: r.get::<_, i64>(3)? != 0,
            })
        })
        .map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        if out.len() >= limit {
            break;
        }
        out.push(row.map_err(sql)?);
    }
    Ok(out)
}

/// How many blocks point at `page` — the folded panel's "and N more".
pub fn count_of(conn: &Connection, page: PageId) -> Result<usize, StorageError> {
    let uri = crate::core::page_uri(page);
    let n: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM marks WHERE kind = 'mention' AND url = ?1)
                  + (SELECT count(*) FROM blocks WHERE page_ref = ?2)",
            params![uri, page.as_u64() as i64],
            |r| r.get(0),
        )
        .map_err(sql)?;
    Ok(n as usize)
}
