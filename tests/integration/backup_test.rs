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
            }),
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
        })
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
