// Full-text index (SPEC §二十, M7): two FTS5 tables — `search_pages`
// (rowid = page id) and `search_blocks` (rowid = block id) — maintained
// inside the *same transaction* as the rows they mirror, so the index can
// never drift from the document (ADR-0014).
//
// One row per searchable source instead of one blob per page: a keystroke
// rewrites exactly one row by rowid, so a debounced typing burst costs
// O(edits), not O(page size) — the 10 000-block page of SPEC §二十二 stays
// cheap to type in.
//
// CJK: `unicode61` never splits inside a run of Han characters, so
// "写作与中文测试" indexes as a single token and a "中文" query misses it.
// Indexing therefore stores a *segmented* copy — every CJK character gets
// its own token — and queries are segmented the same way and issued as a
// phrase, which matches exactly where the substring occurs. ADR-0014 records
// why not the `trigram` tokenizer (it cannot match a one/two-character term).

use rusqlite::{params, Connection, Transaction};

use crate::core::persistence::StorageError;
use crate::core::types::{BlockId, PageId};

/// Character classes to split by hand: CJK ideographs (incl. ext. A and the
/// compatibility block), Japanese kana, Korean syllables. Full-width
/// punctuation is not included — `unicode61` already separates it.
fn is_split_char(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30ff      // Hiragana + Katakana
        | 0x31f0..=0x31ff    // Katakana extensions
        | 0x3400..=0x4dbf    // CJK ext A
        | 0x4e00..=0x9fff    // CJK unified
        | 0xf900..=0xfaff    // CJK compatibility
        | 0xac00..=0xd7af    // Hangul syllables
        | 0x20000..=0x2ebef  // CJK ext B..E
    )
}

/// The indexed form of a text: CJK characters become individual tokens.
pub(crate) fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for ch in text.chars() {
        if is_split_char(ch) {
            out.push(' ');
            out.push(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// True when a token can be written as a bareword (no quoting needed).
fn is_bareword(token: &str) -> bool {
    !token.is_empty()
        && token.chars().all(|c| {
            c.is_alphanumeric() || c == '_' || c == '\'' || is_split_char(c)
        })
}

/// Query tokens: the segmented query split on whitespace, with pieces that
/// hold no indexable character dropped (punctuation-only input must not
/// reach the FTS5 query parser).
fn tokens(query: &str) -> Vec<String> {
    segment(query)
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// The FTS5 MATCH expression for a user query; `None` when the query holds
/// nothing searchable.
///
/// Several terms become a *phrase*, i.e. they must be adjacent — the same
/// substring semantics the blob search had (and what makes Chinese terms
/// work, since the phrase is built from segmented characters). The trailing
/// `*` turns the whole expression into a prefix match, which is what a
/// type-ahead search panel wants; FTS5 allows `*` only after a full phrase
/// ("quick br"* works, "quick br*" does not — the probe in `tests` shows it).
pub fn build_match(query: &str) -> Option<String> {
    let terms = tokens(query);
    if terms.is_empty() {
        return None;
    }
    if terms.len() == 1 {
        let term = &terms[0];
        return Some(if is_bareword(term) {
            format!("{term}*")
        } else {
            format!("\"{term}\"")
        });
    }
    Some(format!("\"{}\"*", terms.join(" ")))
}

fn sql(e: rusqlite::Error) -> StorageError {
    StorageError::Sql(e.to_string())
}

// ── maintenance, called from the repository inside its transaction ────

/// FTS5 `INSERT OR REPLACE` on an explicit rowid: index one page title.
pub(crate) fn index_page_title(
    tx: &Transaction,
    page: PageId,
    title: &str,
) -> Result<(), StorageError> {
    let id = page.as_u64() as i64;
    if title.trim().is_empty() {
        return delete_page_title(tx, page);
    }
    tx.execute(
        "INSERT OR REPLACE INTO search_pages (rowid, title) VALUES (?1, ?2)",
        params![id, segment(title)],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn delete_page_title(tx: &Transaction, page: PageId) -> Result<(), StorageError> {
    tx.execute(
        "DELETE FROM search_pages WHERE rowid = ?1",
        params![page.as_u64() as i64],
    )
    .map_err(sql)?;
    Ok(())
}

/// Index one block's text (or un-index it when empty). The owning page is
/// read from the row that was just written, so callers pass only the id and
/// the new text — one statement per keystroke.
pub(crate) fn upsert_block(tx: &Transaction, id: BlockId, text: &str) -> Result<(), StorageError> {
    if text.is_empty() {
        return delete_block(tx, id);
    }
    tx.execute(
        "INSERT OR REPLACE INTO search_blocks (rowid, page_id, text)
         VALUES (?1, (SELECT page FROM blocks WHERE id = ?1), ?2)",
        params![id.as_u64() as i64, segment(text)],
    )
    .map_err(sql)?;
    Ok(())
}

pub(crate) fn delete_block(tx: &Transaction, id: BlockId) -> Result<(), StorageError> {
    tx.execute(
        "DELETE FROM search_blocks WHERE rowid = ?1",
        params![id.as_u64() as i64],
    )
    .map_err(sql)?;
    Ok(())
}

/// Drop index rows whose source row is gone (FK cascades delete blocks and
/// pages without telling us). Called once per `apply` batch that contained a
/// delete, so subtree deletes need no extra work here. The sweep is O(rows in
/// the index) — deletes are a user action, never per keystroke (SPEC §三十三).
pub(crate) fn prune(tx: &Transaction) -> Result<(), StorageError> {
    tx.execute(
        "DELETE FROM search_blocks WHERE page_id NOT IN (SELECT id FROM pages)
           OR rowid NOT IN (SELECT id FROM blocks)",
        [],
    )
    .map_err(sql)?;
    tx.execute(
        "DELETE FROM search_pages WHERE rowid NOT IN (SELECT id FROM pages)",
        [],
    )
    .map_err(sql)?;
    Ok(())
}

/// Backfill (or refresh) both index tables from the stored rows — used by
/// migration v2, which finds databases that already hold data. Writes go
/// through the same rowid upserts as live maintenance, and the trailing
/// sweeps drop rows whose source has gone missing.
pub fn rebuild(conn: &mut Connection) -> Result<(), StorageError> {
    let tx = conn.transaction().map_err(sql)?;
    let rows = collect(&tx, "SELECT id, title FROM pages")?;
    for (id, text) in rows {
        index_page_title(&tx, PageId(id as u64), &text)?;
    }
    let rows = collect(&tx, "SELECT id, text FROM blocks")?;
    for (id, text) in rows {
        upsert_block(&tx, BlockId(id as u64), &text)?;
    }
    prune(&tx)?;
    tx.commit().map_err(sql)
}

fn collect(tx: &Transaction, sql_str: &str) -> Result<Vec<(i64, String)>, StorageError> {
    let mut stmt = tx.prepare(sql_str).map_err(sql)?;
    let mapped = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(sql)?;
    mapped.collect::<Result<Vec<_>, _>>().map_err(sql)
}

// ── queries ──────────────────────────────────────────────────────────

/// Default cap, matching the search panel's window (SPEC §二十).
pub const DEFAULT_LIMIT: usize = 20;

/// One search request. `page` restricts hits to a single page (in-page
/// find); `limit` caps how many hits the service layer returns (the query
/// itself over-fetches raw matches by a factor of four).
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub page: Option<PageId>,
    pub limit: usize,
}

impl SearchRequest {
    pub fn new(query: &str) -> Self {
        SearchRequest {
            query: query.to_string(),
            page: None,
            limit: DEFAULT_LIMIT,
        }
    }

    pub fn in_page(mut self, page: PageId) -> Self {
        self.page = Some(page);
        self
    }
}

/// One matched source row (a page title or a single block), before any
/// per-page aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub page: PageId,
    /// Title of the matched page, so the caller can label a hit.
    pub page_title: String,
    /// The page title itself matched (rather than one of its blocks).
    pub is_title: bool,
    /// The matched block, when `is_title` is false.
    pub block: Option<BlockId>,
    /// Matched text as stored (unsegmented), for snippet building.
    pub text: String,
    /// SQLite's bm25(): lower (more negative) is a better match.
    pub score: f64,
}

/// Ranked raw matches, best first.
///
/// The `p.template = 0` term in the join is the whole search side of the
/// template feature (SPEC §三十八 "模板"), and it is on the read path rather
/// than the write path on purpose. A template's rows stay *indexed*: excluding
/// them at insert time would mean `insert_block` has to ask whether the page it
/// is filing a block under is a template, which is a second source of truth
/// about a fact one join already has -- and `rebuild` would have to disagree
/// with `insert` about which rows belong in the index, or a template would start
/// appearing in search after a rebuild. One term on the join keeps a template
/// unfindable from both doors.
///
/// Titles and blocks come from their own FTS table, joined back to
/// `pages`/`blocks` for the unsegmented text.
///
/// # Errors
/// Returns [`StorageError`] when the query cannot be prepared or a row cannot be
/// read.
pub fn matches(conn: &Connection, req: &SearchRequest) -> Result<Vec<Match>, StorageError> {
    let Some(expr) = build_match(&req.query) else {
        return Ok(Vec::new());
    };
    // Over-fetch before the caller aggregates per page.
    let fetch = (req.limit.saturating_mul(4)).clamp(20, 500) as i64;
    let page = req.page.map(|p| p.as_u64() as i64);
    let mut stmt = conn
        .prepare(
            "SELECT h.page_id, p.title, h.is_title, h.block,
                    COALESCE(b.text, p.title) AS text, h.score
             FROM (
                SELECT rowid AS page_id, 1 AS is_title, NULL AS block,
                       bm25(search_pages) AS score
                FROM search_pages
                WHERE (?3 IS NULL OR rowid = ?3) AND search_pages MATCH ?1
                UNION ALL
                SELECT page_id, 0, rowid, bm25(search_blocks)
                FROM search_blocks
                WHERE (?3 IS NULL OR page_id = ?3) AND search_blocks MATCH ?1
                ORDER BY score
                LIMIT ?2
             ) AS h
             JOIN pages AS p ON p.id = h.page_id AND p.template = 0
             LEFT JOIN blocks AS b ON b.id = h.block
             ORDER BY h.score
             LIMIT ?2",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map(params![expr, fetch, page], |r| {
            Ok(Match {
                page: PageId(r.get::<_, i64>(0)? as u64),
                page_title: r.get::<_, String>(1)?,
                is_title: r.get::<_, i64>(2)? != 0,
                block: r.get::<_, Option<i64>>(3)?.map(|b| BlockId(b as u64)),
                text: r.get::<_, String>(4)?,
                score: r.get::<_, f64>(5)?,
            })
        })
        .map_err(sql)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(sql)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmentation_gives_each_cjk_character_its_own_token() {
        let segmented = segment("写作test中文");
        let tokens: Vec<&str> = segmented.split_whitespace().collect();
        assert_eq!(tokens, ["写", "作", "test", "中", "文"]);
        assert_eq!(segment("hello world"), "hello world");
        assert_eq!(segment(""), "");
    }

    #[test]
    fn match_query_is_a_segmented_phrase_prefix() {
        // The query is segmented like the index, so a Chinese phrase becomes
        // one token per character and still matches only adjacent text.
        assert_eq!(build_match("字体回退"), Some("\"字 体 回 退\"*".into()));
        assert_eq!(build_match("字体 回退"), Some("\"字 体 回 退\"*".into()));
        assert_eq!(build_match("quick"), Some("quick*".into()));
        assert_eq!(build_match("brown fox"), Some("\"brown fox\"*".into()));
        assert_eq!(build_match("   "), None);
        assert_eq!(build_match("!!!"), None);
        assert_eq!(build_match(""), None);
    }

    /// Manual FTS5 capability probe behind ADR-0014:
    /// `cargo test --lib search_index -- --ignored --nocapture`
    #[test]
    #[ignore = "one-off capability probe, not an assertion"]
    fn fts5_capabilities() {
        let conn = Connection::open_in_memory().unwrap();
        let version: String = conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .unwrap();
        println!("sqlite {version}");
        conn.execute("CREATE VIRTUAL TABLE b USING fts5(page_id UNINDEXED, text)", [])
            .expect("FTS5 must be compiled into the bundled library");
        for (page, id, text) in [
            (1i64, 11i64, "写作与中文测试：字体回退与行高"),
            (2, 21, "The quick brown 中文 fox"),
            (2, 22, "query planning notes"),
        ] {
            conn.execute(
                "INSERT INTO b (rowid, page_id, text) VALUES (?1, ?2, ?3)",
                params![id, page, segment(text)],
            )
            .unwrap();
        }
        let run = |label: &str, expr: &str| {
            let hits: Result<Vec<String>, rusqlite::Error> = conn
                .prepare("SELECT rowid, bm25(b) FROM b WHERE b MATCH ?1 ORDER BY bm25(b)")
                .and_then(|mut s| {
                    s.query_map(params![expr], |r| {
                        Ok(format!("{} ({:.3})", r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                    })
                    .and_then(|rows| rows.collect())
                });
            match hits {
                Ok(v) => println!("  {label:<28} {expr:<28} -> {v:?}"),
                Err(e) => println!("  {label:<28} {expr:<28} !! {e}"),
            }
        };
        for q in [
            "中文",
            "字体回退",
            "quick",
            "bro",
            "brown 中文",
            "测",
            "quer",
            "writing",
        ] {
            let expr = build_match(q).unwrap_or_default();
            run("build_match", &expr);
        }
        run("phrase + prefix tail", "\"quick brown\" \"fo\"*");
        run("phrase then star", "\"quick brown\"*");
        run("cjk phrase then star", "\"字 体 回\"*");
        run("star inside phrase", "\"quick br*\"");
        run("or of phrases", "\"中 文\" OR \"行 高\"");
        run("unsegmented chinese", "\"中文\"");
        run("prefix of a longer word", "bro*");
        conn.execute("CREATE VIRTUAL TABLE g USING fts5(s, tokenize='trigram')", [])
            .map(|_| {
                conn.execute("INSERT INTO g(rowid, s) VALUES (1, '写作与中文测试')", [])
                    .unwrap();
                for q in ["中文", "中文测", "quer"] {
                    let v: Result<Vec<i64>, rusqlite::Error> = conn
                        .prepare("SELECT rowid FROM g WHERE g MATCH ?1")
                        .and_then(|mut s| {
                            s.query_map(params![q], |r| r.get::<_, i64>(0))?
                                .collect()
                        });
                    println!("  trigram {q:?} -> {v:?}");
                }
            })
            .map_err(|e| println!("  trigram unavailable: {e}"))
            .ok();
    }
}
