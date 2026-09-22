// Named versions of one page (SPEC §三十八 "version history", ADR-0091).
//
// SPEC §三十八 says version history reuses §二十五's snapshot mechanism and
// must not invent a second store. Read literally, that is what this file is: a
// version is a database file made by `VACUUM INTO` over the live connection —
// the same statement §二十五's `backup::snapshot` runs, with the same
// durability bargain (the copy is expendable, so it is written with
// `synchronous=OFF`) and the same validation (it is read back by opening it
// with `Database::open`, which migrates and integrity-checks it). What a
// version adds over a `.bak<N>` is a name, and a page, and the fact that
// rotation must not eat it.
//
// So a version is the library's snapshot *filtered down to one page*: after
// the copy is written, every other page is deleted from it and the two FTS
// tables are emptied — emptied with FTS5's `delete-all` command rather than a
// `DELETE`, because a plain delete leaves the term index behind (see
// `clear_index`). The delete is one statement because the schema's foreign keys
// cascade (`blocks.page … ON DELETE CASCADE`, and `block_children` / `marks`
// cascade from `blocks`), so the rows this page does not own go with their
// owners and nothing here has to know the table list. Emptying the search index
// matters for the same reason: it is a *copy of the text of every page*, and
// leaving it in would make "a snapshot of one page" quietly be "a copy of the
// library" at the byte level. The FTS tables are derived data (§三十七's
// language for exactly this kind of thing), so dropping them costs nothing that
// a rebuild cannot put back — and a rebuild never reads this file.
//
// Two consequences of the file being an ordinary Quire database are worth
// stating because they are the reason no new reader code exists: opening a
// version hands back a page with its title, its marks, its colours, its code
// language, its table grid and its attachment ids through `Repository::load`,
// i.e. the same loader the app starts with. A version cannot drift from the
// format, because it *is* the format.
//
// The index — which versions exist, and what the user called them — lives in
// the live database's own `metadata` table, one row per version
// (`version/<page>/<created>` = the label) plus one companion row listing the
// attachment ids that version's rows point at (`version-files/<page>/<created>`
// = "7,12"). That keeps the promise the reclaim scan had to be given for covers
// (ADR-0047): a pointer the database cannot answer is a pointer the reclaim will
// delete, and a version that outlives its page's picture would restore as a
// missing image with nothing to explain it. Two meta rows are cheaper than one
// query that has to open a hundred files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};

use crate::core::persistence::{Repository, StorageError};
use crate::core::types::{Block, Page, PersistedState};

use super::database::Database;
use super::repository::SqliteRepository;

/// How many named versions one page keeps. Beyond this the oldest go away, and
/// the panel says so rather than refusing the save: a cap that silently drops
/// the newest entry is worse than one that drops the oldest, and a refusal with
/// no way to free room is a dead end. Twenty answers "the last few drafts of
/// this week" for a page whose snapshot is measured in tens of kilobytes; see
/// docs/PERFORMANCE.md for the bytes this bounds.
pub const MAX_PER_PAGE: usize = 20;

/// The prefix of the metadata rows that name a version. `-files` hangs off the
/// same idea, and neither is a prefix of the other, so a scan for one cannot
/// pick up the other by accident.
pub const KEY_LABEL: &str = "version/";
pub const KEY_FILES: &str = "version-files/";

/// Folder holding version files, beside the `attachments` folder and the
/// database: one library's own files stay together, and the §三十七 reclaim
/// story never has to look inside a folder it does not own.
pub fn folder(db_file: &Path) -> PathBuf {
    db_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join("versions")
}

/// `<folder>/p<page>-<created>.db`. Numbers only, because a file name is the
/// worst place to store text the user typed: the label belongs to the metadata
/// row, where a name with a slash, an emoji or both works.
pub fn path_for(db_file: &Path, page: i64, created: i64) -> PathBuf {
    folder(db_file).join(format!("p{page}-{created}.db"))
}

pub fn label_key(page: i64, created: i64) -> String {
    format!("{KEY_LABEL}{page}/{created}")
}

pub fn files_key(page: i64, created: i64) -> String {
    format!("{KEY_FILES}{page}/{created}")
}

/// `(page, created)` out of a label key, or `None` when the row is not one
/// this feature wrote. A hand-edited or half-deleted row reads as absent, which
/// is what keeps a stray from being restored as a page zero.
pub fn parse_key(key: &str) -> Option<(i64, i64)> {
    let rest = key.strip_prefix(KEY_LABEL)?;
    let (page, created) = rest.split_once('/')?;
    Some((page.parse().ok()?, created.parse().ok()?))
}

/// The attachment ids a `version-files/` value lists. An unparseable entry is
/// skipped rather than failing the load — the cost of skipping is a picture the
/// reclaim keeps, which is the direction this scan has always erred in.
pub fn parse_ids(value: &str) -> BTreeSet<i64> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

/// One page's version files, newest first, as `(created, label)`.
pub fn index_of(persisted: &PersistedState, page: i64) -> Vec<(i64, String)> {
    let mut found: Vec<(i64, String)> = persisted
        .meta
        .iter()
        .filter_map(|(key, value)| {
            let (owner, created) = parse_key(key)?;
            (owner == page).then(|| (created, value.clone()))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
}

/// Empty one FTS5 table for good.
///
/// A plain `DELETE FROM <fts>` is not enough: FTS5 keeps its term dictionary
/// and postings in a `<table>_data` b-tree that the delete leaves behind.
/// Measured on a 201-page library, a version file whose `search_blocks` reported
/// 0 rows still carried 1.4 MB of `_data` — which is precisely the "a snapshot
/// of one page that is quietly a copy of the library" this narrowing exists to
/// prevent. `DELETE` empties the content and docsize shadow tables, and the
/// `rebuild` command then re-derives `_data` from them, i.e. from nothing.
/// (`delete-all` would be shorter, and SQLite refuses it here: it is only for
/// contentless or external-content tables, and this index stores its content.)
fn clear_index(conn: &rusqlite::Connection, fts: &str) -> rusqlite::Result<usize> {
    conn.execute(&format!("DELETE FROM {fts}"), [])?;
    conn.execute(&format!("INSERT INTO {fts}({fts}) VALUES ('rebuild')"), [])
}

/// Write `page` out as a version file and hand back the attachment ids its rows
/// point at, which the caller records beside the label (§三十七's reclaim).
///
/// `created` is the caller's clock; a second version taken in the same second
/// would collide on the file name and the metadata key, so `None` reports the
/// collision and the caller retries a second later rather than this function
/// picking a time of its own.
pub fn save(
    repo: &SqliteRepository,
    page: i64,
    created: i64,
) -> Result<Option<Vec<i64>>, StorageError> {
    let Some(db_file) = repo.path().filter(|_| page > 0) else {
        return Err(StorageError::Open(
            "this session has no database file, so a version has nowhere to live".into(),
        ));
    };
    let target = path_for(db_file, page, created);
    if target.exists() {
        return Ok(None);
    }
    let dir = folder(db_file);
    std::fs::create_dir_all(&dir)
        .map_err(|e| StorageError::Sql(format!("create {}: {e}", dir.display())))?;

    // The copy. Same statement, same lock and the same trade as
    // `backup::snapshot`: reading through the live connection sees pages
    // already committed into the WAL, and the durability switch is off because
    // a torn version is one the next save overwrites, while every real write
    // still runs at FULL.
    {
        let conn = repo.database().conn();
        conn.pragma_update(None, "synchronous", "OFF")
            .map_err(|e| StorageError::Sql(format!("version synchronous=OFF: {e}")))?;
        let vacuum = conn.execute("VACUUM INTO ?1", params![target.display().to_string()]);
        if let Err(e) = conn.pragma_update(None, "synchronous", "FULL") {
            eprintln!("quire: could not restore synchronous=FULL ({e})");
        }
        match vacuum {
            Ok(_) | Err(rusqlite::Error::ExecuteReturnedResults) => {}
            Err(e) => {
                let _ = std::fs::remove_file(&target);
                return Err(StorageError::Sql(format!("version snapshot to {}: {e}", target.display())));
            }
        }
    }

    // Narrow the copy to the one page, then shrink the file to what is left.
    let ids = {
        let vdb = match Database::open(&target) {
            Ok(db) => db,
            Err(e) => {
                let _ = std::fs::remove_file(&target);
                return Err(e);
            }
        };
        let conn = vdb.conn();
        conn.execute("DELETE FROM pages WHERE id != ?1", params![page])
            .and_then(|_| clear_index(&conn, "search_pages"))
            .and_then(|_| clear_index(&conn, "search_blocks"))
            .map_err(|e| StorageError::Sql(format!("narrow version file: {e}")))?;
        let exists: Option<i64> = conn
            .query_row("SELECT id FROM pages WHERE id = ?1", params![page], |r| r.get(0))
            .optional()
            .map_err(|e| StorageError::Sql(format!("read version page: {e}")))?;
        if exists.is_none() {
            drop(conn);
            drop(vdb);
            let _ = std::fs::remove_file(&target);
            return Err(StorageError::Sql(format!(
                "page {page} is not in the database, so there is nothing to version"
            )));
        }
        let mut ids: Vec<i64> = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT DISTINCT attachment FROM blocks WHERE attachment IS NOT NULL")
                .map_err(|e| StorageError::Sql(format!("read version attachments: {e}")))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, i64>(0))
                .map_err(|e| StorageError::Sql(format!("read version attachments: {e}")))?;
            for row in rows {
                ids.push(row.map_err(|e| StorageError::Sql(format!("read version attachments: {e}")))?);
            }
        }
        // `VACUUM` rebuilds the file, which is what turns "the library, minus
        // most of it" into a small file: deleting rows frees pages inside the
        // file but does not hand the space back.
        conn.execute("VACUUM", [])
            .map_err(|e| StorageError::Sql(format!("shrink version file: {e}")))?;
        ids
    };
    Ok(Some(ids))
}

/// The page and the block sequence a version file holds, through the same
/// loader the app opens the library with. Blocks come back in display order,
/// which is the one thing `load` does not promise and this reader has to: the
/// panel reads top to bottom, and a restore re-derives keys anyway.
pub fn read(db_file: &Path, page: i64, created: i64) -> Result<(Page, Vec<Block>), StorageError> {
    let target = path_for(db_file, page, created);
    let repo = SqliteRepository::from_database(Database::open(&target)?);
    let state = repo.load()?;
    let pages: Vec<Page> = state.pages.into_iter().filter(|p| p.id.0 as i64 == page).collect();
    let [one] = pages.as_slice() else {
        return Err(StorageError::Corrupt(format!(
            "{} holds {} pages, not one",
            target.display(),
            pages.len()
        )));
    };
    let mut blocks: Vec<Block> = state
        .blocks
        .into_iter()
        .filter(|b| b.page.0 as i64 == page)
        .collect();
    blocks.sort_by_key(|b| b.order.0);
    Ok((one.clone(), blocks))
}

/// Remove a version's file. The metadata rows are the caller's business,
/// because they travel through the change feed like every other write.
pub fn remove(db_file: &Path, page: i64, created: i64) -> Result<(), StorageError> {
    for suffix in ["", "-wal", "-shm"] {
        let mut path = path_for(db_file, page, created).into_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                StorageError::Sql(format!("remove {}: {e}", path.display()))
            })?;
        }
    }
    Ok(())
}

/// Delete version files the index no longer names — one left behind by a save
/// that died between the copy and the metadata row, or by a library someone
/// edited by hand. `keep` is every `(page, created)` the index still lists.
///
/// This is the folder's housekeeping, so it takes the whole set and not one
/// page: the point is that no file in `versions/` is invisible forever.
pub fn sweep_orphans(
    db_file: &Path,
    keep: &BTreeSet<(i64, i64)>,
) -> Result<Vec<PathBuf>, StorageError> {
    let dir = folder(db_file);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new()); // no folder yet: nothing written has been orphaned
    };
    let mut gone = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Only this folder's own version files, and not the WAL sidecars that
        // belong to one: a stray here is somebody's other data, and this
        // function gets no vote on files it did not name.
        let Some(stem) = name.strip_prefix('p').and_then(|s| s.strip_suffix(".db")) else {
            continue;
        };
        let Some((page, created)) = stem.split_once('-') else {
            continue;
        };
        let (Ok(page), Ok(created)) = (page.parse::<i64>(), created.parse::<i64>()) else {
            continue;
        };
        if !keep.contains(&(page, created)) {
            remove(db_file, page, created)?;
            gone.push(path);
        }
    }
    Ok(gone)
}
