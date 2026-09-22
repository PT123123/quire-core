// M8 crash-recovery acceptance tests (SPEC §二十五): the rotating `.bak<N>`
// family written at every open, its retention windows (D10 — newest `KEEP`
// generations and `MAX_AGE`), and the startup path that restores a corrupt
// main file from the newest snapshot that still opens.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use quire::core::persistence::{Change, Repository, StorageError};
use quire::core::types::{Block, BlockId, BlockKind, Lang, OrderKey, Page, PageFont, PageId};
use quire::services::search_service::SearchService;
use quire::services::settings_store::{Settings, SettingsStore};
use quire::storage::backup::{self, KEEP};
use quire::storage::{Database, SqliteRepository};
use quire::testing::ScratchDir;

/// A workspace folder that goes away with the test. Each `tempdir()` call used
/// to be answered by a hand-written `remove_dir_all(&dir).unwrap()` on the
/// happy path only, so every red run left another one behind.
fn tempdir() -> ScratchDir {
    ScratchDir::new("backup")
}

/// Write a file's modified time, which is the only clock `prune` can read: a
/// retention test that waited out the age window would take a week.
fn set_modified(path: &Path, when: SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
}

/// Overwrite the body of a database file, keeping the header recognizable —
/// the damage `PRAGMA integrity_check` is supposed to catch.
fn corrupt(path: &Path) {
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    let mut bytes = std::fs::read(path).unwrap();
    let from = bytes.len() / 2;
    for b in &mut bytes[from..] {
        *b = 0xAB;
    }
    std::fs::write(path, &bytes).unwrap();
}

fn write_marker(path: &Path, value: &str) {
    let repo = SqliteRepository::open(path).unwrap();
    repo.apply(&[Change::SettingSet {
        key: "marker".into(),
        value: value.into(),
    }])
    .unwrap();
    drop(repo);
}

fn marker_at(path: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row("SELECT value FROM settings WHERE key = 'marker'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
}

fn marker_in(repo: &SqliteRepository) -> Option<String> {
    repo.load()
        .unwrap()
        .settings
        .get("marker")
        .cloned()
}

#[test]
fn every_open_rotates_the_snapshot_family() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    // One session more than the family holds: the oldest marker has to have
    // been shifted into the last generation, and nothing beyond it.
    let sessions = KEEP + 1;
    for session in 1..=sessions {
        let repo = SqliteRepository::open(&path).unwrap();
        // the snapshot is taken before this session writes, so `.bak1` on the
        // next open holds exactly the previous sessions
        if session < sessions {
            repo.apply(&[Change::SettingSet {
                key: "marker".into(),
                value: format!("session-{session}"),
            }])
            .unwrap();
        }
        drop(repo);
    }
    for index in 1..=KEEP {
        let expected = format!("session-{}", sessions - index);
        assert_eq!(
            Some(expected.as_str()),
            marker_at(&backup::slot(&path, index)).as_deref(),
            "generation {index}"
        );
    }
    assert!(
        !backup::slot(&path, KEEP + 1).exists(),
        "the family stays bounded at {KEEP} generations"
    );
}

/// Age out the middle of the family and prune must drop exactly that
/// generation: the count window alone would keep it, and deleting a snapshot
/// a weekly-open user still needs would be worse than leaving one behind.
#[test]
fn prune_drops_a_generation_older_than_the_age_window() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "content"); // this open writes `.bak1`

    let now = SystemTime::now();
    let week_old = now - Duration::from_secs(8 * 24 * 60 * 60);
    let mut aged = Vec::new();
    for index in [2usize, 4] {
        let file = backup::slot(&path, index);
        std::fs::write(&file, b"snapshot").unwrap();
        set_modified(&file, week_old);
        aged.push(file);
    }
    let fresh: Vec<PathBuf> = [1usize, 3, 5]
        .iter()
        .map(|index| backup::slot(&path, *index))
        .collect();
    for file in &fresh {
        if !file.exists() {
            std::fs::write(file, b"snapshot").unwrap();
        }
    }

    let removed = backup::prune(&path, now).unwrap();
    assert_eq!(aged, removed);
    for file in &fresh {
        assert!(file.exists(), "{file:?} is inside both windows");
    }

    // and the next open rewrites the family it just trimmed
    write_marker(&path, "content");
    assert!(backup::slot(&path, 1).exists());
}

#[test]
fn prune_bounds_a_family_an_older_setting_left_behind() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    for index in 1..=KEEP + 2 {
        std::fs::write(backup::slot(&path, index), b"snapshot").unwrap();
    }
    let removed = backup::prune(&path, SystemTime::now()).unwrap();
    assert_eq!(
        vec![backup::slot(&path, KEEP + 1), backup::slot(&path, KEEP + 2)],
        removed,
        "everything from 1..=KEEP is insurance recover still walks"
    );
    // a second pass is quiet: nothing left to say, nothing to delete
    assert!(backup::prune(&path, SystemTime::now()).unwrap().is_empty());
}

#[test]
fn a_snapshot_of_a_workspace_with_old_snapshots_keeps_the_new_window() {
    // The retention policy runs from inside `snapshot`, so a normal session
    // cannot leave both windows open at once: the aged generation is dropped
    // while the fresh `.bak1` and its younger neighbours stay.
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "one"); // .bak1 = the empty first database
    write_marker(&path, "two"); // .bak1 = "one", .bak2 = empty
    // rotation renames, so the aged file lands one generation down
    let aged = backup::slot(&path, 2);
    let when = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
    set_modified(&aged, when);

    write_marker(&path, "three");
    assert_eq!(
        Some("three"),
        marker_at(&path).as_deref(),
        "pruning the family must leave the main database alone"
    );
    assert_eq!(Some("two"), marker_at(&backup::slot(&path, 1)).as_deref());
    assert_eq!(Some("one"), marker_at(&backup::slot(&path, 2)).as_deref());
    assert!(
        !backup::slot(&path, 3).exists(),
        "the aged generation is gone"
    );
}

#[test]
fn a_snapshot_is_one_self_contained_file() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "content");
    write_marker(&path, "content");
    let newest = backup::slot(&path, 1);
    assert!(newest.exists());
    for suffix in ["-wal", "-shm"] {
        let sidecar = format!("{}{suffix}", newest.display());
        assert!(
            !Path::new(&sidecar).exists(),
            "VACUUM INTO must leave no journal behind next to {sidecar}"
        );
    }
    // and it opens on its own, integrity check and all
    assert_eq!(Some("content"), marker_at(&newest).as_deref());
}

#[test]
fn a_corrupt_main_file_is_restored_from_the_newest_snapshot() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "keep-me"); // session 1
    write_marker(&path, "keep-me"); // session 2 snapshots session 1's state
    corrupt(&path);
    assert!(
        matches!(Database::open(&path), Err(StorageError::Corrupt(_))),
        "the startup check must see the damage"
    );

    let repo = SqliteRepository::open(&path).expect("recovery must succeed");
    assert_eq!(Some("keep-me"), marker_in(&repo).as_deref());
    // the corpse is kept for a manual `sqlite3 .recover`, under a new name
    assert!(Path::new(&format!("{}.corrupt", path.display())).exists());
    // and the recovered state is snapshotted again at once
    assert_eq!(Some("keep-me"), marker_at(&backup::slot(&path, 1)).as_deref());
    drop(repo);
}

#[test]
fn recovery_skips_a_snapshot_that_is_also_damaged() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "oldest");
    write_marker(&path, "newer");
    write_marker(&path, "newest");
    // `.bak1` holds "newer", `.bak2` holds "oldest"; damage the newest one
    corrupt(&backup::slot(&path, 1));
    corrupt(&path);

    let repo = SqliteRepository::open(&path).expect("the older snapshot must work");
    assert_eq!(Some("oldest"), marker_in(&repo).as_deref());
    drop(repo);
}

#[test]
fn with_no_usable_snapshot_the_corrupt_error_and_the_file_both_stay() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "gone");
    for index in 1..=KEEP {
        let _ = std::fs::remove_file(backup::slot(&path, index));
    }
    corrupt(&path);
    let before = std::fs::read(&path).unwrap();

    let error = match SqliteRepository::open(&path) {
        Ok(_) => panic!("nothing is recoverable here"),
        Err(e) => e,
    };
    assert!(
        matches!(error, StorageError::Corrupt(_)),
        "got {error}"
    );
    assert_eq!(before, std::fs::read(&path).unwrap());
    assert!(!Path::new(&format!("{}.corrupt", path.display())).exists());
}

#[test]
fn a_recovered_database_is_searchable_straight_away() {
    // the FTS5 mirror travels inside the snapshot, so recovery brings the
    // index with it (ADR-0014) — no rebuild before the first query
    let dir = tempdir();
    let path = dir.join("workspace.db");
    {
        let repo = SqliteRepository::open(&path).unwrap();
        repo.apply(&[
            Change::PageCreated(Page {
                id: PageId(1),
                title: "Notes".into(),
                parent: None,
                order: OrderKey(10),
                favorite: false,
                expanded: false,
                font: PageFont::default(),
                full_width: false,
                small_text: false,
                icon: String::new(),
                cover: None,
                locked: false,
                template: false,
            }),
            Change::BlockInserted(Block {
                id: BlockId(11),
                page: PageId(1),
                parent: None,
                order: OrderKey(10),
                kind: BlockKind::Paragraph,
                text: "字体回退与行高".into(),
                checked: false,
            marks: Vec::new(),
        color: quire::core::ColorKind::Default,
        background: quire::core::ColorKind::Default,
        page_ref: None,
        folded: false,
        attachment: None,
        img_percent: 100,
        columns: 0,
        lang: Lang::Plain,
        db_ref: None,
        sync_ref: None,            }),
        ])
        .unwrap();
        drop(repo);
    }
    // this session's own edit is deliberately not in the snapshot: `.bak1` is
    // the database as of the open, which is the documented loss window
    write_marker(&path, "lost-on-recovery");
    corrupt(&path);

    let repo = Arc::new(SqliteRepository::open(&path).unwrap());
    let service = SearchService::new(repo.clone());
    let hits = service.query_in_page("字体", PageId(1)).unwrap();
    assert_eq!(1, hits.len());
    assert_eq!(
        Some(BlockId(11)),
        hits[0].block,
        "the indexed block came back with the file"
    );
    assert_eq!(None, marker_in(&repo).as_deref());
    drop(service);
    drop(repo);
}

#[test]
fn settings_round_trip_through_the_sqlite_backend() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    {
        let repo = Arc::new(SqliteRepository::open(&path).unwrap());
        let store = SettingsStore::new(repo);
        let mut settings = Settings::new();
        settings.set_theme("dark");
        settings.set_expanded_pages(&[PageId(7), PageId(2)]);
        store.save_settings(&settings).unwrap();
        let mut meta = Settings::new();
        meta.set("last.page", "7");
        store.save_meta(&meta).unwrap();
    }

    // a second session on the same file reads both namespaces back
    let reopened = Arc::new(SqliteRepository::open(&path).unwrap());
    let store = SettingsStore::new(reopened.clone());
    let loaded = store.load_settings().unwrap();
    assert_eq!(Some("dark"), loaded.theme());
    assert_eq!(vec![PageId(2), PageId(7)], loaded.expanded_pages());
    assert_eq!(Some("7"), store.load_meta().unwrap().get("last.page"));

    // a removal reaches the file as a real delete and reads back absent
    let mut smaller = loaded.clone();
    smaller.remove(Settings::KEY_EXPANDED);
    store.save_settings(&smaller).unwrap();
    let after = store.load_settings().unwrap();
    assert!(after.expanded_pages().is_empty());
    assert_eq!(Some("dark"), after.theme());
    assert!(
        !reopened.load().unwrap().settings.contains_key(Settings::KEY_EXPANDED),
        "the row is gone from the table, not just hidden"
    );
    drop(store);
    drop(reopened);
}

#[test]
fn a_clean_open_reports_nothing() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    let (repo, report) = SqliteRepository::open_with_report(&path).unwrap();
    assert!(
        report.is_clean(),
        "a first open restores nothing and backs everything up: {report:?}"
    );
    drop(repo);
    // writing over the same file again is equally quiet
    write_marker(&path, "still-fine");
    let (repo, report) = SqliteRepository::open_with_report(&path).unwrap();
    assert!(report.is_clean(), "{report:?}");
    drop(repo);
}

#[test]
fn the_report_names_the_snapshot_it_rolled_back_to() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "keep-me");
    write_marker(&path, "keep-me");
    corrupt(&path);

    let (repo, report) = SqliteRepository::open_with_report(&path).unwrap();
    assert_eq!(Some(backup::slot(&path, 1)), report.recovered_from);
    assert!(!report.backup_failed);
    assert_eq!(Some("keep-me"), marker_in(&repo).as_deref());
    drop(repo);
}

#[test]
fn the_report_says_when_no_snapshot_could_be_written() {
    let dir = tempdir();
    let path = dir.join("workspace.db");
    write_marker(&path, "content");
    // rotation has to delete the oldest generation first; a directory sitting
    // there (non-empty, so even a recursive delete would refuse) fails the
    // snapshot without touching the main file
    let blocked = backup::slot(&path, KEEP);
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("in-the-way"), b"x").unwrap();

    let (repo, report) = SqliteRepository::open_with_report(&path).unwrap();
    assert!(report.backup_failed, "the snapshot was blocked: {report:?}");
    assert_eq!(None, report.recovered_from);
    assert_eq!(Some("content"), marker_in(&repo).as_deref());
    drop(repo);
}

/// One-off cost probe for docs/PERFORMANCE.md, not an assertion: what the
/// startup snapshot costs against a workspace the size of scene D. Run with
/// `cargo test --test backup -- --ignored --nocapture`.
#[test]
#[ignore = "one-off cost probe, not an assertion"]
fn snapshot_cost() {
    use quire::core::types::PersistedState;
    use std::time::Instant;

    let dir = tempdir();
    let path = dir.join("cost.db");
    let pages: Vec<Page> = (1..=1_000)
        .map(|i| Page {
            id: PageId(i),
            title: format!("page {i}"),
            parent: None,
            order: OrderKey(i as u64 * 10),
            favorite: false,
            expanded: false,
            font: PageFont::default(),
            full_width: false,
            small_text: false,
            icon: String::new(),
            cover: None,
            locked: false,
            template: false,
        })
        .collect();
    let blocks: Vec<Block> = (1..=10_000)
        .map(|i| Block {
            id: BlockId(i),
            page: PageId(1 + i % 1_000),
            parent: None,
            order: OrderKey(i as u64 * 10),
            kind: BlockKind::Paragraph,
            text: format!("block {i} carrying a little text to index"),
            checked: false,
            marks: Vec::new(),
        color: quire::core::ColorKind::Default,
        background: quire::core::ColorKind::Default,
        page_ref: None,
        folded: false,
        attachment: None,
        img_percent: 100,
        columns: 0,
        lang: Lang::Plain,
        db_ref: None,
        sync_ref: None,        })
        .collect();
    {
        let repo = SqliteRepository::open(&path).unwrap();
        repo.replace_all(&PersistedState {
            pages,
            blocks,
            ..PersistedState::default()
        })
        .unwrap();
        drop(repo);
    }
    let size = std::fs::metadata(&path).unwrap().len();
    let journal: String = {
        let probe = rusqlite::Connection::open(&path).unwrap();
        let page_size: i64 = probe
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        let autocheckpoint: i64 = probe
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .unwrap();
        let mut text = String::new();
        for pragma in ["journal_mode", "synchronous", "locking_mode"] {
            // `synchronous` answers as an integer, the rest as text
            let value: rusqlite::types::Value = probe
                .query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))
                .unwrap();
            text.push_str(&format!("{pragma}={value:?} "));
        }
        format!("{text}page_size={page_size} wal_autocheckpoint={autocheckpoint}")
    };
    println!("workspace pragmas: {journal}");
    for round in 1..=3 {
        let started = Instant::now();
        let db = Database::open(&path).unwrap();
        let plain = started.elapsed();
        // the shipped path: durability relaxed for the expendable copy
        let started = Instant::now();
        backup::snapshot(&db, &path).unwrap();
        let snapshot = started.elapsed();
        // the same statement at the connection's normal durability, for the
        // A/B recorded in docs/PERFORMANCE.md
        let comparator = dir.join(format!("full-{round}.bak"));
        let outer = rusqlite::Connection::open(&path).unwrap();
        let started = Instant::now();
        outer
            .execute(
                "VACUUM INTO ?1",
                rusqlite::params![comparator.display().to_string()],
            )
            .unwrap();
        let at_full = started.elapsed();
        let backup_bytes = std::fs::metadata(backup::slot(&path, 1)).unwrap().len();
        println!(
            "round {round}: main {size} bytes — open {plain:.3?}, snapshot {snapshot:.3?} \
             (backup {backup_bytes} bytes), same copy at synchronous=FULL {at_full:.3?}"
        );
    }
    let started = Instant::now();
    let repo = SqliteRepository::open(&path).unwrap();
    let opened = started.elapsed();
    let blocks = repo.load().unwrap().blocks.len();
    println!("repository open (rotate + snapshot): {opened:.3?}, {blocks} blocks loaded");
    drop(repo);
}

// Named versions of one page (SPEC §三十八, ADR-0050). These are the file-level
// promises: a version is §二十五's `VACUUM INTO` narrowed to one page, so the
// tests below ask whether the narrowing really happened — in the rows, in the
// search index, and in the bytes on disk — and whether the folder's own
// housekeeping can tell its files from somebody else's.
mod version_files {
    use super::{tempdir, SqliteRepository};
    use quire::core::persistence::{Change, Repository, StorageError};
    use quire::core::types::{
        Attachment, AttachmentId, Block, BlockId, BlockKind, ColorKind, Lang, Mark, MarkKind,
        OrderKey, Page, PageFont, PageId, PersistedState,
    };
    use quire::storage::versions as v;
    use std::collections::BTreeSet;

    fn page(id: u64, title: &str) -> Page {
        Page {
            id: PageId(id),
            title: title.into(),
            parent: None,
            order: OrderKey(id * 10),
            favorite: false,
            expanded: true,
            font: PageFont::default(),
            full_width: false,
            small_text: false,
            icon: String::new(),
            cover: None,
            locked: false,
            template: false,
        }
    }

    fn para(id: u64, owner: u64, text: &str) -> Block {
        Block {
            id: BlockId(id),
            page: PageId(owner),
            parent: None,
            order: OrderKey(id * 10),
            kind: BlockKind::Paragraph,
            text: text.into(),
            checked: false,
            marks: Vec::new(),
            color: ColorKind::Default,
            background: ColorKind::Default,
            page_ref: None,
            folded: false,
            attachment: None,
            img_percent: 100,
            columns: 0,
            lang: Lang::Plain,
            db_ref: None,
            sync_ref: None,
        }
    }

    /// A library with three pages and enough text on each that "one page, minus
    /// the search index" is measurably smaller than "the library".
    fn library(path: &std::path::Path) -> SqliteRepository {
        let repo = SqliteRepository::open(path).unwrap();
        let mut changes: Vec<Change> = Vec::new();
        for p in 1..=3u64 {
            changes.push(Change::PageCreated(page(p, &format!("Page {p}"))));
            for i in 0..120u64 {
                let id = p * 1000 + i;
                changes.push(Change::BlockInserted(para(
                    id,
                    p,
                    &format!("page {p} line {i} — enough words to be worth indexing here"),
                )));
            }
        }
        repo.apply(&changes).unwrap();
        repo
    }

    fn count(path: &std::path::Path, sql: &str) -> i64 {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap()
    }

    fn ids(values: &[i64]) -> BTreeSet<i64> {
        values.iter().copied().collect()
    }

    #[test]
    fn a_version_is_the_library_narrowed_to_one_page() {
        let dir = tempdir();
        let path = dir.join("workspace.db");
        let repo = library(&path);
        let created = 1_700_000_000;
        assert_eq!(
            v::save(&repo, 2, created),
            Ok(Some(vec![])),
            "no block of page 2 points at a file, so the pin list is empty"
        );

        let file = v::path_for(&path, 2, created);
        assert!(file.exists(), "the version is a file beside the library");
        for suffix in ["-wal", "-shm"] {
            assert!(
                !std::path::Path::new(&format!("{}{suffix}", file.display())).exists(),
                "a version has to be one self-contained file, like a snapshot"
            );
        }

        assert_eq!(
            count(&file, "SELECT COUNT(*) FROM pages"),
            1,
            "every other page is gone, and the cascade took its blocks with it"
        );
        assert_eq!(count(&file, "SELECT COUNT(*) FROM blocks"), 120);
        // The FTS tables are a copy of every page's text. Leaving them in would
        // make "a snapshot of one page" quietly be "a copy of the library".
        assert_eq!(count(&file, "SELECT COUNT(*) FROM search_pages"), 0);
        assert_eq!(count(&file, "SELECT COUNT(*) FROM search_blocks"), 0);
        // …and the row counts alone do not prove it. FTS5 keeps its terms in a
        // `<table>_data` b-tree that a plain `DELETE` leaves standing: measured
        // on a 201-page library, a version with 0 rows in `search_blocks` still
        // held 368 rows / 1.4 MB of `search_blocks_data` — the words of every
        // page it was not allowed to contain. An emptied index sits at two rows
        // (its root and config cells), so a handful here is the real check.
        for fts in ["search_pages", "search_blocks"] {
            let terms = count(&file, &format!("SELECT COUNT(*) FROM {fts}_data"));
            assert!(
                terms <= 4,
                "{fts}_data holds {terms} rows: the term index survived the narrowing"
            );
        }

        // The library's own rows are still in its WAL at this moment — `VACUUM
        // INTO` reads through the connection, which sees them, while the main
        // file on disk has not taken them yet — so the comparison is with every
        // byte the library occupies.
        let mut main = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            main += std::fs::metadata(format!("{}{suffix}", path.display()))
                .map(|m| m.len())
                .unwrap_or(0);
        }
        let version = std::fs::metadata(&file).unwrap().len();
        assert!(
            version * 2 < main,
            "one page of three must be less than half the library it came from: {version} vs {main}"
        );
    }

    #[test]
    fn a_version_reads_back_through_the_loader_the_app_uses() {
        // The reason no second reader exists: a version file *is* a Quire
        // database, so everything a page can carry survives the round trip.
        let dir = tempdir();
        let path = dir.join("workspace.db");
        let repo = SqliteRepository::open(&path).unwrap();
        let mut block = para(11, 1, "Ship it");
        block.kind = BlockKind::Callout;
        block.color = ColorKind::Blue;
        block.background = ColorKind::Yellow;
        block.lang = Lang::Rust;
        block.checked = true;
        block.folded = true;
        block.attachment = Some(AttachmentId(4));
        block.img_percent = 50;
        block.marks = vec![Mark {
            start: 0,
            end: 4,
            kind: MarkKind::Bold,
            url: String::new(),
            date: None,
        }];
        repo.apply(&[
            Change::PageCreated(Page {
                title: "A page with a name".into(),
                icon: "🚀".into(),
                ..page(1, "unused")
            }),
            Change::AttachmentAdded(Attachment {
                id: AttachmentId(4),
                name: "Sunset photo.png".into(),
                file: "4.png".into(),
                thumb: String::new(),
                mime: "image/png".into(),
                bytes: 12345,
                width: 1280,
                height: 720,
            }),
            Change::BlockInserted(block.clone()),
            Change::BlockInserted(para(12, 1, "second line")),
        ])
        .unwrap();

        let pinned = v::save(&repo, 1, 1_700_000_001).unwrap().unwrap();
        assert_eq!(pinned, vec![4], "the picture a version points at is recorded");

        let (stored, blocks) = v::read(&path, 1, 1_700_000_001).unwrap();
        assert_eq!(stored.title, "A page with a name");
        assert_eq!(stored.icon, "🚀");
        assert_eq!(blocks, vec![block, para(12, 1, "second line")]);
    }

    #[test]
    fn a_second_version_taken_in_the_same_second_is_refused_not_overwritten() {
        let dir = tempdir();
        let path = dir.join("workspace.db");
        let repo = library(&path);
        assert!(v::save(&repo, 1, 1_700_000_002).unwrap().is_some());
        let before = std::fs::metadata(v::path_for(&path, 1, 1_700_000_002))
            .unwrap()
            .len();
        assert_eq!(
            v::save(&repo, 1, 1_700_000_002),
            Ok(None),
            "`None` is the collision the caller retries a second later"
        );
        let after = std::fs::metadata(v::path_for(&path, 1, 1_700_000_002))
            .unwrap()
            .len();
        assert_eq!(before, after, "and the first copy is untouched by the refusal");
    }

    #[test]
    fn a_version_of_a_page_that_is_not_there_leaves_no_file() {
        let dir = tempdir();
        let path = dir.join("workspace.db");
        let repo = library(&path);
        let result = v::save(&repo, 99, 1_700_000_003);
        assert!(
            matches!(result, Err(StorageError::Sql(_))),
            "a page nobody can name is an error, not an empty version: {result:?}"
        );
        assert!(!v::path_for(&path, 99, 1_700_000_003).exists());
        // and a session with no database file says so at the door
        let memory = SqliteRepository::in_memory().unwrap();
        assert!(
            v::save(&memory, 1, 1).is_err(),
            "no file, so nowhere for a version to live"
        );
    }

    #[test]
    fn the_index_lists_one_pages_versions_newest_first() {
        let persisted = PersistedState {
            pages: Vec::new(),
            blocks: Vec::new(),
            meta: [
                (v::label_key(1, 200), "Draft".to_string()),
                (v::label_key(1, 300), "Shipped".to_string()),
                (v::label_key(2, 400), "Other page".to_string()),
                (v::files_key(1, 200), "7,12".to_string()),
                // strangers: a half-written key, a key missing its number, and
                // a row this feature never writes
                ("version/x/1".to_string(), "junk".to_string()),
                ("version/1".to_string(), "junk".to_string()),
                ("something/else".to_string(), "junk".to_string()),
            ]
            .into_iter()
            .collect(),
            settings: Default::default(),
        };
        assert_eq!(
            v::index_of(&persisted, 1),
            vec![(300, "Shipped".to_string()), (200, "Draft".to_string())],
            "newest first, and nothing that is not a label row"
        );
        assert_eq!(v::index_of(&persisted, 2).len(), 1);
        assert_eq!(v::index_of(&persisted, 3), Vec::new());
        // `-files` hangs off the same idea without being one of its prefixes
        assert_eq!(v::parse_key(&v::files_key(1, 200)), None);
        assert_eq!(v::parse_ids("7,12"), ids(&[7, 12]));
        assert_eq!(
            v::parse_ids("7, ,oops,12"),
            ids(&[7, 12]),
            "an unparseable id is skipped, which costs a picture the reclaim keeps"
        );
    }

    #[test]
    fn the_folder_sweep_takes_only_what_the_index_no_longer_names() {
        let dir = tempdir();
        let path = dir.join("workspace.db");
        let repo = library(&path);
        v::save(&repo, 1, 100).unwrap();
        v::save(&repo, 1, 200).unwrap();
        v::save(&repo, 2, 300).unwrap();
        // a file a crashed save left behind: written, never listed
        std::fs::copy(v::path_for(&path, 1, 100), v::path_for(&path, 1, 999)).unwrap();
        // and something that is not this folder's business at all
        let stranger = v::folder(&path).join("readme.txt");
        std::fs::write(&stranger, b"not a version").unwrap();

        let keep: BTreeSet<(i64, i64)> = [(1i64, 200i64), (2, 300)].into_iter().collect();
        let gone = v::sweep_orphans(&path, &keep).unwrap();
        assert_eq!(gone.len(), 2, "the pruned one and the orphan: {gone:?}");
        assert!(!v::path_for(&path, 1, 100).exists());
        assert!(!v::path_for(&path, 1, 999).exists());
        assert!(v::path_for(&path, 1, 200).exists());
        assert!(
            v::path_for(&path, 2, 300).exists(),
            "another page's version is kept"
        );
        assert!(
            stranger.exists(),
            "a file this folder did not name is nobody's to delete"
        );

        // no folder yet is not an error — a library that never versioned
        let fresh = tempdir();
        let empty = fresh.join("new.db");
        let _repo = SqliteRepository::open(&empty).unwrap();
        assert_eq!(v::sweep_orphans(&empty, &keep).unwrap(), Vec::<std::path::PathBuf>::new());
    }

    #[test]
    fn removing_a_version_takes_its_sidecars_with_it() {
        let dir = tempdir();
        let path = dir.join("workspace.db");
        let repo = library(&path);
        v::save(&repo, 1, 12345).unwrap();
        let file = v::path_for(&path, 1, 12345);
        for suffix in ["-wal", "-shm"] {
            std::fs::write(format!("{}{suffix}", file.display()), b"x").unwrap();
        }
        v::remove(&path, 1, 12345).unwrap();
        assert!(!file.exists());
        for suffix in ["-wal", "-shm"] {
            assert!(
                !std::path::Path::new(&format!("{}{suffix}", file.display())).exists(),
                "a version is one file, and a half-deleted one is a leak"
            );
        }
        // removing what is already gone is a no-op, like `MetaDelete`
        v::remove(&path, 1, 12345).unwrap();
    }

    /// One-off cost probe for docs/PERFORMANCE.md, not an assertion: what one
    /// page's version file costs against the library it came from, what the
    /// twenty-per-page cap bounds that at, and how long the copy takes. Run with
    /// `cargo test --test backup -- --ignored --nocapture`.
    #[test]
    #[ignore = "one-off cost probe, not an assertion"]
    fn version_cost() {
        use std::time::Instant;

        fn on_disk(path: &std::path::Path) -> u64 {
            ["", "-wal", "-shm"]
                .iter()
                .map(|suffix| {
                    std::fs::metadata(format!("{}{suffix}", path.display()))
                        .map(|m| m.len())
                        .unwrap_or(0)
                })
                .sum()
        }

        let dir = tempdir();
        let path = dir.join("cost.db");
        let repo = SqliteRepository::open(&path).unwrap();
        // A working library: 200 pages of 60 lines, plus one page the size of a
        // long one (5 000 lines), which is the worst case the cap bounds.
        let mut changes: Vec<Change> = Vec::new();
        for p in 1..=200u64 {
            changes.push(Change::PageCreated(page(p, &format!("Page {p}"))));
            for i in 0..60u64 {
                changes.push(Change::BlockInserted(para(
                    p * 1000 + i,
                    p,
                    &format!("page {p} line {i} — enough words to be worth indexing here"),
                )));
            }
        }
        changes.push(Change::PageCreated(page(900, "The long one")));
        for i in 0..5_000u64 {
            changes.push(Change::BlockInserted(para(
                900_000 + i,
                900,
                &format!("long page line {i} — enough words to be worth indexing here"),
            )));
        }
        repo.apply(&changes).unwrap();
        let main = on_disk(&path);
        let rows = count(&path, "SELECT COUNT(*) FROM blocks");
        println!("library: {main} bytes on disk, {rows} blocks in 201 pages");
        // The RAM half of the question is bounded by two sizes, so print the one
        // that is not a constant: a version's rows are `Block`s, and a diff holds
        // a second copy of them.
        println!(
            "one Block in memory: {} bytes, so the 5 000-line page is {} bytes of rows while a version is open",
            std::mem::size_of::<Block>(),
            5_000 * std::mem::size_of::<Block>()
        );

        for (label, p) in [("an ordinary 60-line page", 7u64), ("a 5 000-line page", 900u64)] {
            let started = Instant::now();
            v::save(&repo, p as i64, 1_700_000_000).unwrap();
            let took = started.elapsed();
            let bytes = std::fs::metadata(v::path_for(&path, p as i64, 1_700_000_000))
                .unwrap()
                .len();
            println!(
                "{label}: version {bytes} bytes, {}% of the library",
                bytes * 100 / main
            );
            println!("    copy + narrow + vacuum took {took:.3?}");
            std::fs::remove_file(v::path_for(&path, p as i64, 1_700_000_000)).unwrap();
        }

        // The cap's worst case: twenty versions of the long page.
        let started = Instant::now();
        for i in 0..20i64 {
            v::save(&repo, 900, 1_700_000_100 + i).unwrap();
        }
        let took = started.elapsed();
        let folder = v::folder(&path);
        let total: u64 = std::fs::read_dir(&folder)
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::metadata(e.path()).ok())
            .map(|m| m.len())
            .sum();
        let files = std::fs::read_dir(&folder).unwrap().count();
        println!(
            "20 versions of the long page: {total} bytes in {files} files, {took:.3?} ({:.1?} each)",
            took / 20
        );
        println!("    that is {}% of the library", total * 100 / main);
    }
}
