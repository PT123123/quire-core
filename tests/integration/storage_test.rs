// M3 storage acceptance tests: round-trip over the Repository contract,
// migrations, transactional rollback, and crash behaviour (kill the
// writer mid-transaction, reopen; plus a committed-then-killed fixture).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};

use quire::core::persistence::{Change, Repository, StorageError};
use quire::core::types::{
    Block, BlockId, BlockKind, OrderKey, Page, PageId, PersistedState,
};
use quire::storage::backup;
use quire::storage::migrations;
use quire::storage::SqliteRepository;

fn page(id: u64, title: &str, parent: Option<u64>, ord: u64) -> Page {
    Page {
        id: PageId(id),
        title: title.into(),
        parent: parent.map(PageId),
        order: OrderKey(ord),
        favorite: false,
        expanded: false,
    }
}

fn block(id: u64, page_id: u64, parent: Option<u64>, ord: u64, text: &str) -> Block {
    Block {
        id: BlockId(id),
        page: PageId(page_id),
        parent: parent.map(BlockId),
        order: OrderKey(ord),
        kind: BlockKind::Paragraph,
        text: text.into(),
        checked: false,
    }
}

fn sample_state() -> PersistedState {
    let mut meta = BTreeMap::new();
    meta.insert("schema_note".to_string(), "m3".to_string());
    let mut settings = BTreeMap::new();
    settings.insert("dark".to_string(), "true".to_string());
    settings.insert("locale".to_string(), "zh-CN".to_string());
    PersistedState {
        pages: vec![
            page(1, "Getting started", None, 1 << 32),
            // children before parent on purpose: bulk insert must not care
            page(3, "Nested deep", Some(2), 20),
            page(2, "Sub page", Some(1), 10),
            page(4, "Favorite 收藏", None, (1u64 << 63) + 7),
        ],
        blocks: vec![
            block(10, 1, None, 100, "first"),
            Block {
                kind: BlockKind::Todo,
                checked: true,
                ..block(12, 1, Some(10), 110, "child todo")
            },
            Block {
                kind: BlockKind::Code,
                ..block(11, 1, None, (1u64 << 63) + 500, "let x = 1; // 代码")
            },
            block(20, 2, None, 1, "only block"),
            block(40, 4, None, u64::MAX - 1, "top of the ordinal range"),
            block(41, 4, None, u64::MAX, "last possible key"),
        ],
        meta,
        settings,
    }
}

fn sorted_pages(state: &PersistedState) -> Vec<Page> {
    let mut v = state.pages.clone();
    v.sort_by_key(|p| p.id);
    v
}

fn sorted_blocks(state: &PersistedState) -> Vec<Block> {
    let mut v = state.blocks.clone();
    v.sort_by_key(|b| b.id);
    v
}

#[test]
fn replace_all_and_load_round_trip() {
    let repo = SqliteRepository::in_memory().unwrap();
    let state = sample_state();
    repo.replace_all(&state).unwrap();
    let loaded = repo.load().unwrap();
    assert_eq!(sorted_pages(&loaded), sorted_pages(&state));
    assert_eq!(sorted_blocks(&loaded), sorted_blocks(&state));
    assert_eq!(loaded.meta, state.meta);
    assert_eq!(loaded.settings, state.settings);
}

#[test]
fn file_database_survives_reopen() {
    let dir = tempfile();
    let path = dir.join("quire.db");
    let state = sample_state();
    {
        let repo = SqliteRepository::open(&path).unwrap();
        repo.replace_all(&state).unwrap();
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let loaded = repo.load().unwrap();
    assert_eq!(sorted_pages(&loaded), sorted_pages(&state));
    assert_eq!(loaded.settings.get("locale").map(String::as_str), Some("zh-CN"));
}

#[test]
fn change_streams_apply_as_programmed() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.replace_all(&sample_state()).unwrap();

    repo.apply(&[
        Change::PageCreated(page(5, "Fresh", Some(1), 15)),
        Change::PageTitleSet {
            id: PageId(5),
            title: "Fresh title".into(),
        },
        Change::PageFavoriteSet {
            id: PageId(5),
            favorite: true,
        },
        Change::PageExpandedSet {
            id: PageId(1),
            expanded: true,
        },
        Change::PageMoved {
            id: PageId(5),
            parent: None,
            order: OrderKey(999),
        },
        Change::BlockInserted(block(50, 5, None, 1 << 32, "hello 编辑")),
        Change::BlockTextSet {
            id: BlockId(50),
            text: "hello world".into(),
        },
        Change::BlockKindSet {
            id: BlockId(50),
            kind: BlockKind::Heading2,
        },
        Change::BlockCheckedSet {
            id: BlockId(50),
            checked: true,
        },
        Change::BlockMoved {
            id: BlockId(50),
            parent: Some(BlockId(10)),
            order: OrderKey(105),
        },
        Change::MetaSet {
            key: "last_page_id".into(),
            value: "5".into(),
        },
        Change::SettingSet {
            key: "dark".into(),
            value: "false".into(),
        },
    ])
    .unwrap();

    let loaded = repo.load().unwrap();
    let p5 = loaded.pages.iter().find(|p| p.id == PageId(5)).unwrap();
    assert_eq!(p5.title, "Fresh title");
    assert_eq!(p5.parent, None);
    assert_eq!(p5.order, OrderKey(999));
    assert!(p5.favorite);
    let b50 = loaded.blocks.iter().find(|b| b.id == BlockId(50)).unwrap();
    assert_eq!(b50.text, "hello world");
    assert_eq!(b50.kind, BlockKind::Heading2);
    assert!(b50.checked);
    assert_eq!(b50.parent, Some(BlockId(10)));
    assert_eq!(b50.order, OrderKey(105));
    assert_eq!(loaded.meta.get("last_page_id").map(String::as_str), Some("5"));
    assert_eq!(loaded.settings.get("dark").map(String::as_str), Some("false"));
    assert_eq!(loaded.pages.len(), 5);
    assert_eq!(loaded.blocks.len(), 7);
}

#[test]
fn deletes_cascade_recursively() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.replace_all(&sample_state()).unwrap();

    // block 10 has child 12; deleting 10 must take 12 with it
    repo.apply(&[Change::BlockDeleted { id: BlockId(10) }]).unwrap();
    let loaded = repo.load().unwrap();
    assert!(!loaded.blocks.iter().any(|b| b.id == BlockId(10) || b.id == BlockId(12)));
    assert!(loaded.blocks.iter().any(|b| b.id == BlockId(11)));

    // page 1 has sub-page 2 which has sub-page 3 and blocks of its own
    repo.apply(&[Change::PageDeleted { id: PageId(1) }]).unwrap();
    let loaded = repo.load().unwrap();
    for gone in [1u64, 2, 3] {
        assert!(!loaded.pages.iter().any(|p| p.id == PageId(gone)));
    }
    assert!(!loaded.blocks.iter().any(|b| b.page == PageId(1) || b.page == PageId(2)));
    assert!(loaded.pages.iter().any(|p| p.id == PageId(4)));
}

#[test]
fn failing_batch_rolls_the_whole_thing_back() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.replace_all(&sample_state()).unwrap();
    let before = repo.load().unwrap();

    let err = repo
        .apply(&[
            Change::BlockTextSet {
                id: BlockId(10),
                text: "should not stick".into(),
            },
            // orphan block -> FK violation at the latest point of no return
            Change::BlockInserted(block(900, 900, None, 1, "nowhere page")),
        ])
        .unwrap_err();
    assert!(matches!(err, StorageError::Sql(_)), "got {err}");

    let after = repo.load().unwrap();
    assert_eq!(sorted_blocks(&after), sorted_blocks(&before));

    // an update aimed at a missing id is an error, not a silent no-op
    let err = repo
        .apply(&[Change::BlockTextSet {
            id: BlockId(123456),
            text: "x".into(),
        }])
        .unwrap_err();
    assert!(matches!(err, StorageError::Sql(_)));
}

#[test]
fn migrations_upgrade_a_v0_database_and_are_idempotent() {
    let dir = tempfile();
    let path = dir.join("migrate.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE legacy (id INTEGER PRIMARY KEY, junk TEXT);
             INSERT INTO legacy VALUES (1, 'from the future that was v0');
             PRAGMA user_version = 0;",
        )
        .unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), migrations::CURRENT_VERSION);
        // legacy rows survive an upgrade
        let junk: String = conn
            .query_row("SELECT junk FROM legacy WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(junk, "from the future that was v0");
        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), migrations::CURRENT_VERSION);
    }
    // a repository opening the same file sees an already-current schema
    let repo = SqliteRepository::open(&path).unwrap();
    assert!(repo.load().unwrap().pages.is_empty());
}

#[test]
fn replace_all_rejects_a_parent_cycle_at_commit() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.replace_all(&sample_state()).unwrap();
    let mut broken = sample_state();
    // 1 -> 2 -> 1 would pass immediate FK checks in insert order, the
    // deferred commit check must still refuse the whole batch.
    broken.pages.iter_mut().for_each(|p| match p.id {
        PageId(1) => p.parent = Some(PageId(2)),
        PageId(2) => p.parent = Some(PageId(1)),
        _ => {}
    });
    assert!(repo.replace_all(&broken).is_err());
    // the good state is untouched
    assert_eq!(sorted_pages(&repo.load().unwrap()), sorted_pages(&sample_state()));
}

#[test]
fn corrupt_database_is_reported_at_startup() {
    let dir = tempfile();
    let path = dir.join("corrupt.db");
    {
        let repo = SqliteRepository::open(&path).unwrap();
        repo.replace_all(&sample_state()).unwrap();
    }
    // wipe the middle of the file (page content, header stays recognizable)
    let mut bytes = std::fs::read(&path).unwrap();
    let half = bytes.len() / 2;
    for b in &mut bytes[half..] {
        *b = 0x5A;
    }
    std::fs::write(&path, &bytes).unwrap();
    // the opening above also wrote a snapshot family; without it there is
    // nothing to fall back to. Recovery itself is covered by backup_test.
    for index in 1..=backup::KEEP {
        let _ = std::fs::remove_file(backup::slot(&path, index));
    }
    let err = match SqliteRepository::open(&path) {
        Ok(_) => panic!("a corrupt database must not open"),
        Err(e) => e,
    };
    assert!(
        matches!(err, StorageError::Corrupt(_)),
        "expected Corrupt, got {err}"
    );
}

#[test]
fn unknown_block_kind_is_corruption_on_load() {
    let dir = tempfile();
    let path = dir.join("badkind.db");
    {
        let repo = SqliteRepository::open(&path).unwrap();
        repo.replace_all(&sample_state()).unwrap();
    }
    {
        // forge a row no Quire write could ever produce
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE blocks SET kind = 'spreadsheet' WHERE id = 11", [])
            .unwrap();
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let err = repo.load().unwrap_err();
    assert!(matches!(err, StorageError::Corrupt(_)), "got {err}");
}

// ── crash tests ─────────────────────────────────────────────────────

/// A batch that fails halfway must leave the earlier confirmed data
/// exactly as it was — the rollback half of the kill -9 story (the
/// forward half is `killed_writer_leaves_confirmed_data_intact`).
#[test]
fn mid_batch_failure_keeps_confirmed_data() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.apply(&[Change::PageCreated(page(1, "Killed", None, 10))])
        .unwrap();
    repo.apply(&[Change::BlockInserted(block(2, 1, None, 20, "confirmed"))])
        .unwrap();

    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        repo.apply(&[
            Change::BlockTextSet {
                id: BlockId(2),
                text: "rewritten".into(),
            },
            // FK violation on purpose: the transaction must die with it
            Change::BlockInserted(block(3, 999, None, 30, "ghost")),
        ])
        .unwrap();
    }));
    assert!(err.is_err(), "the bad batch must fail");

    let loaded = repo.load().unwrap();
    let b = loaded.blocks.iter().find(|b| b.id == BlockId(2)).unwrap();
    assert_eq!(b.text, "confirmed", "confirmed data must not be lost");
    assert!(!loaded.blocks.iter().any(|blk| blk.id == BlockId(3)));
}

#[test]
fn killed_writer_leaves_confirmed_data_intact() {
    let dir = tempfile();
    let path = dir.join("crash.db");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "crash_writer_child"])
        .env("QUIRE_CRASH_PORT", port.to_string())
        .env("QUIRE_CRASH_DB", &path)
        // libtest output in the child would only interleave; the handshake
        // over TCP is the real progress signal.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn test child");

    // wait for the child's "committed" handshake
    let handshake = std::time::Duration::from_secs(30);
    let got = listener
        .incoming()
        .next()
        .expect("no incoming connection")
        .and_then(|mut stream| {
            stream.set_read_timeout(Some(handshake))?;
            let mut buf = [0u8; 32];
            let n = stream.read(&mut buf)?;
            Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
        })
        .ok();
    assert_eq!(got.as_deref(), Some("committed"), "child handshake failed");

    // SIGKILL equivalent: hard process termination, no Drop runs,
    // no rollback, WAL/journal files stay as the crash left them.
    child.kill().unwrap();
    child.wait().unwrap();

    let repo = SqliteRepository::open(&path).expect("database must reopen after the crash");
    let state = repo.load().unwrap();
    assert!(
        state.pages.iter().any(|p| p.id == PageId(1) && p.title == "Kept"),
        "committed page lost: {state:?}"
    );
    assert!(
        state.blocks.iter().any(|b| b.id == BlockId(2) && b.text == "before the crash"),
        "committed block lost: {state:?}"
    );
    // the aborted transaction's rows may never surface
    assert!(
        !state.pages.iter().any(|p| p.id == PageId(777)),
        "in-flight page became visible: {:?}",
        state.pages
    );
    assert!(
        !state.blocks.iter().any(|b| b.text.contains("never committed")),
        "partial write became visible: {:?}",
        state.blocks
    );
}

#[test]
#[ignore = "child process for killed_writer_leaves_confirmed_data_intact"]
fn crash_writer_child() {
    let port = std::env::var("QUIRE_CRASH_PORT")
        .expect("QUIRE_CRASH_PORT")
        .parse::<u16>()
        .expect("port");
    let db = std::env::var("QUIRE_CRASH_DB").expect("QUIRE_CRASH_DB");

    let repo = SqliteRepository::open(Path::new(&db)).unwrap();
    repo.apply(&[
        Change::PageCreated(page(1, "Kept", None, 10)),
        Change::BlockInserted(block(2, 1, None, 20, "before the crash")),
    ])
    .unwrap();

    let mut stream = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    stream.write_all(b"committed\n").unwrap();
    stream.flush().unwrap();

    // Leave a transaction permanently in flight: BEGIN + rows, never
    // committed. The parent's kill is then deterministic — SQLite WAL
    // recovery must discard exactly these rows on the next open.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         INSERT INTO pages (id, title, parent, ord, favorite, expanded)
             VALUES (777, 'in flight', NULL, 1, 0, 0);
         INSERT INTO blocks (id, page, kind, text, checked)
             VALUES (777, 777, 'paragraph', 'never committed', 0);
         INSERT INTO block_children (block, parent, ord) VALUES (777, NULL, 1);",
    )
    .unwrap();

    // Sit inside the transaction until killed; self-terminate after two
    // minutes in case the parent ever dies before reaching kill().
    for _ in 0..120 {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    std::process::exit(0x7F);
}

// ── helpers ─────────────────────────────────────────────────────────

/// Manual latency probe for docs/PERFORMANCE.md:
/// `cargo test --test storage -- --ignored --nocapture save_latency`
#[test]
#[ignore = "measurement probe, not an assertion"]
fn save_latency() {
    use std::time::Instant;

    let dir = tempfile();
    let path = dir.join("latency.db");
    let repo = SqliteRepository::open(&path).unwrap();

    let mut seed = sample_state();
    for id in 5..=1004u64 {
        seed.pages.push(page(id, "latency page", None, id * 10));
        for b in 0..10 {
            seed.blocks.push(block(
                id * 1000 + b,
                id,
                None,
                (b + 1) * 100,
                "the quick brown fox jumps over the lazy dog 中文内容",
            ));
        }
    }
    let t = Instant::now();
    repo.replace_all(&seed).unwrap();
    println!("startup load side:");
    let tl = Instant::now();
    let n = repo.load().unwrap();
    println!("  load {} blocks + {} pages: {:?}", n.blocks.len(), n.pages.len(), tl.elapsed());
    println!("bulk replace_all ({} blocks): {:?}", seed.blocks.len(), t.elapsed());

    // a typical debounced burst: 30 text updates of one page + a setting
    let mut batch: Vec<Change> = (0..30)
        .map(|i| Change::BlockTextSet {
            id: BlockId(5_001),
            text: format!("typed up to {i} — 输入内容"),
        })
        .collect();
    batch.push(Change::SettingSet {
        key: "last_draft".into(),
        value: "1".into(),
    });
    let times: Vec<std::time::Duration> = (0..20)
        .map(|_| {
            let t = Instant::now();
            repo.apply(&batch).unwrap();
            t.elapsed()
        })
        .collect();
    let total: std::time::Duration = times.iter().sum();
    println!(
        "apply 32-change batch (WAL, synchronous=FULL): min {:?} median {:?} mean {:?} max {:?}",
        times.iter().min().unwrap(),
        {
            let mut s = times.clone();
            s.sort();
            s[s.len() / 2]
        },
        total / times.len() as u32,
        times.iter().max().unwrap()
    );
}

fn tempfile() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "quire-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

static NEXT_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
