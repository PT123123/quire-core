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
    Attachment, AttachmentId, Block, BlockId, BlockKind, Lang, OrderKey, Page, PageFont, PageId,
    PersistedState,
};
use quire::storage::backup;
use quire::storage::migrations;
use quire::storage::SqliteRepository;
use quire::testing::ScratchDir;

fn page(id: u64, title: &str, parent: Option<u64>, ord: u64) -> Page {
    Page {
        id: PageId(id),
        title: title.into(),
        parent: parent.map(PageId),
        order: OrderKey(ord),
        favorite: false,
        expanded: false,
        font: PageFont::default(),
        full_width: false,
        small_text: false,
        icon: String::new(),
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
        marks: Vec::new(),
        color: quire::core::ColorKind::Default,
        background: quire::core::ColorKind::Default,
        page_ref: None,
        folded: false,
        attachment: None,
        img_percent: 100,
        columns: 0,
        lang: Lang::Plain,
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
                marks: Vec::new(),
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
fn meta_and_setting_deletes_remove_their_rows() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.replace_all(&sample_state()).unwrap();
    repo.apply(&[
        Change::MetaSet {
            key: "last_session_aborted".into(),
            value: "panicked somewhere".into(),
        },
        Change::SettingSet {
            key: "theme".into(),
            value: "dark".into(),
        },
    ])
    .unwrap();
    let loaded = repo.load().unwrap();
    assert_eq!(
        loaded.meta.get("last_session_aborted").map(String::as_str),
        Some("panicked somewhere")
    );
    assert_eq!(loaded.settings.get("theme").map(String::as_str), Some("dark"));

    // the consumer drains a key without a prior read; deleting an absent
    // key is a no-op, so the same batch is safe to retry
    repo.apply(&[
        Change::MetaDelete {
            key: "last_session_aborted".into(),
        },
        Change::MetaDelete {
            key: "never_written".into(),
        },
        Change::SettingDelete { key: "theme".into() },
    ])
    .unwrap();
    let loaded = repo.load().unwrap();
    assert!(!loaded.meta.contains_key("last_session_aborted"));
    assert!(!loaded.settings.contains_key("theme"));
}

#[test]
fn page_block_reference_round_trips() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.replace_all(&sample_state()).unwrap();
    repo.apply(&[
        Change::BlockInserted(block(60, 1, None, 300, "Untitled")),
        Change::BlockKindSet {
            id: BlockId(60),
            kind: BlockKind::Page,
        },
        Change::BlockRefSet {
            id: BlockId(60),
            page: Some(PageId(2)),
        },
    ])
    .unwrap();
    let state = repo.load().unwrap();
    let b = state.blocks.iter().find(|b| b.id == BlockId(60)).unwrap();
    assert_eq!(b.kind, BlockKind::Page);
    assert_eq!(b.page_ref, Some(PageId(2)));
    // clearing the reference persists too, and other blocks stay untouched
    repo.apply(&[Change::BlockRefSet {
        id: BlockId(60),
        page: None,
    }])
    .unwrap();
    let state = repo.load().unwrap();
    let b = state.blocks.iter().find(|b| b.id == BlockId(60)).unwrap();
    assert_eq!(b.page_ref, None);
    assert_eq!(b.kind, BlockKind::Page);
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

/// The v5 -> v6 step is a conditional ALTER, so it has to be tested against
/// a database that really is missing the column, not a fresh one.
#[test]
fn the_v6_step_adds_folded_to_a_v5_database() {
    let dir = tempfile();
    let path = dir.join("fold.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        // roll the schema back to what version 5 shipped
        conn.execute_batch(
            "ALTER TABLE blocks DROP COLUMN folded; PRAGMA user_version = 5;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages (id, title, parent, ord, favorite, expanded)
             VALUES (1, 'Old', NULL, 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blocks (id, page, kind, text, checked, color, bg)
             VALUES (10, 1, 'paragraph', 'from v5', 0, '', '')",
            [],
        )
        .unwrap();
        // block_children rows are what load() joins on
        conn.execute("INSERT INTO block_children (block, parent, ord) VALUES (10, NULL, 100)", [])
            .unwrap();
        drop(conn);

        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), 5);
        // control: the fixture must really be missing the column, or this
        // test would pass without the v6 step ever running
        let pre: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'folded'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pre, 0, "the rolled-back schema has no folded column");
        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), migrations::CURRENT_VERSION);
        // the pre-upgrade row reads as unfolded rather than failing the load
        let folded: i64 = conn
            .query_row("SELECT folded FROM blocks WHERE id = 10", [], |r| r.get(0))
            .unwrap();
        assert_eq!(folded, 0);
        migrations::ensure_current(&mut conn).unwrap();
    }
    SqliteRepository::open(&path).unwrap().load().unwrap();
}

/// Like the v6 step, v7 is a conditional ALTER plus a CREATE TABLE, so it has
/// to be tested against a database that really lacks both.
#[test]
fn the_v7_step_adds_attachments_to_a_v6_database() {
    let dir = tempfile();
    let path = dir.join("attach.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        // roll the schema back to what version 6 shipped
        conn.execute_batch(
            "DROP TABLE attachments;
             ALTER TABLE blocks DROP COLUMN attachment;
             ALTER TABLE blocks DROP COLUMN img_percent;
             PRAGMA user_version = 6;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages (id, title, parent, ord, favorite, expanded)
             VALUES (1, 'Old', NULL, 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blocks (id, page, kind, text, checked, color, bg, folded)
             VALUES (10, 1, 'paragraph', 'from v6', 0, '', '', 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO block_children (block, parent, ord) VALUES (10, NULL, 100)", [])
            .unwrap();
        drop(conn);

        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), 6);
        // control: all three things v7 adds must really be absent, or this
        // test would pass without the step ever running
        let tables: i64 = conn
            .query_row("SELECT count(*) FROM pragma_table_info('attachments')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, 0, "the rolled-back schema has no attachments table");
        for column in ["attachment", "img_percent"] {
            let present: i64 = conn
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = ?1",
                    [column],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(present, 0, "the rolled-back schema has no {column} column");
        }

        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), migrations::CURRENT_VERSION);
        // the pre-upgrade block reads as a plain paragraph with a default
        // width, not as a corrupt row
        let (att, pct): (Option<i64>, i64) = conn
            .query_row(
                "SELECT attachment, img_percent FROM blocks WHERE id = 10",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(att, None);
        assert_eq!(pct, 100);
        migrations::ensure_current(&mut conn).unwrap();
        migrations::check_schema(&conn).unwrap();
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let state = repo.load().unwrap();
    let old = state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap();
    assert_eq!(old.attachment, None);
    assert_eq!(old.img_percent, 100);
    assert!(repo.load_attachments().unwrap().is_empty());
}

#[test]
fn a_grid_and_its_cells_round_trip_through_storage() {
    use quire::core::{Mark, MarkKind};
    let dir = tempfile();
    let path = dir.join("grid.db");
    let table = Block {
        kind: BlockKind::Table,
        columns: 3,
        ..block(50, 1, None, 90, "")
    };
    let cell = |id: u64, ord: u64, text: &str| {
        Block { kind: BlockKind::TableCell, ..block(id, 1, Some(50), ord, text) }
    };
    let state = PersistedState {
        pages: vec![page(1, "Grid", None, 1)],
        blocks: vec![
            table.clone(),
            cell(51, 91, "North"),
            Block {
                marks: vec![Mark { start: 0, end: 5, kind: MarkKind::Bold, url: String::new() }],
                ..cell(52, 92, "South")
            },
            cell(53, 93, "East"),
            cell(54, 94, "West"),
            cell(55, 95, "Up"),
            cell(56, 96, "Down"),
        ],
        meta: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    SqliteRepository::open(&path).unwrap().replace_all(&state).unwrap();
    let loaded = SqliteRepository::open(&path).unwrap().load().unwrap();
    // the grid is its table plus six children, parent and column count intact
    assert_eq!(sorted_blocks(&loaded), sorted_blocks(&state));
    let back = loaded.blocks.iter().find(|b| b.id == BlockId(50)).unwrap();
    assert_eq!((back.kind, back.columns), (BlockKind::Table, 3));
    assert_eq!(back.text, "");
    let cells: Vec<&Block> =
        loaded.blocks.iter().filter(|b| b.parent == Some(BlockId(50))).collect();
    assert_eq!(cells.len(), 6);
    assert_eq!(
        cells[1].marks,
        vec![Mark { start: 0, end: 5, kind: MarkKind::Bold, url: String::new() }],
        "a cell's inline marks are ordinary block marks"
    );
    // the kind strings on disk are the ones the SPEC names, because the
    // Markdown export and any outside reader match on them
    let conn = rusqlite::Connection::open(&path).unwrap();
    let kinds: Vec<String> = conn
        .prepare("SELECT kind FROM blocks WHERE kind LIKE 'table%' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["table", "table_cell", "table_cell", "table_cell", "table_cell", "table_cell", "table_cell"]
    );
    let columns: i64 =
        conn.query_row("SELECT columns FROM blocks WHERE id = 50", [], |r| r.get(0)).unwrap();
    assert_eq!(columns, 3);
}

/// A columns layout stores exactly like a grid: the container carries the
/// box count in the same `columns` column, and the boxes and their lines are
/// ordinary parent-linked child blocks. That is why slice B needed no new
/// migration.
#[test]
fn a_columns_layout_and_its_boxes_round_trip_through_storage() {
    let dir = tempfile();
    let path = dir.join("columns.db");
    let layout = Block { kind: BlockKind::Columns, columns: 2, ..block(60, 1, None, 90, "") };
    let box_of =
        |id: u64, ord: u64| Block { kind: BlockKind::Column, ..block(id, 1, Some(60), ord, "") };
    let state = PersistedState {
        pages: vec![page(1, "Layout", None, 1)],
        blocks: vec![
            block(59, 1, None, 80, "before the layout"),
            layout.clone(),
            box_of(61, 91),
            box_of(62, 92),
            Block { kind: BlockKind::Todo, checked: true, ..block(63, 1, Some(61), 93, "left") },
            block(64, 1, Some(62), 94, "right"),
            block(65, 1, None, 95, "after the layout"),
        ],
        meta: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    SqliteRepository::open(&path).unwrap().replace_all(&state).unwrap();
    let loaded = SqliteRepository::open(&path).unwrap().load().unwrap();
    assert_eq!(sorted_blocks(&loaded), sorted_blocks(&state));
    let back = loaded.blocks.iter().find(|b| b.id == BlockId(60)).unwrap();
    assert_eq!((back.kind, back.columns), (BlockKind::Columns, 2));
    let boxes: Vec<&Block> =
        loaded.blocks.iter().filter(|b| b.parent == Some(BlockId(60))).collect();
    assert_eq!(boxes.len(), 2, "the layout owns its boxes directly");
    assert!(boxes.iter().all(|b| b.kind == BlockKind::Column));
    let left = loaded.blocks.iter().find(|b| b.id == BlockId(63)).unwrap();
    assert_eq!((left.parent, left.checked), (Some(BlockId(61)), true), "a box keeps its lines");
    // the kind strings on disk are the ones the SPEC names
    let conn = rusqlite::Connection::open(&path).unwrap();
    let kinds: Vec<String> = conn
        .prepare("SELECT kind FROM blocks WHERE kind IN ('columns', 'column') ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(kinds, ["columns", "column", "column"]);
    let columns: i64 =
        conn.query_row("SELECT columns FROM blocks WHERE id = 60", [], |r| r.get(0)).unwrap();
    assert_eq!(columns, 2, "the box count lives in the v8 column");
    // and a box is a box only through its kind: it stores no extra state
    let box_columns: i64 =
        conn.query_row("SELECT columns FROM blocks WHERE id = 61", [], |r| r.get(0)).unwrap();
    assert_eq!(box_columns, 0);
}

/// v8 is a conditional ALTER like v6 and v7, so it has to run against a
/// database that really lacks the column.
#[test]
fn the_v8_step_adds_columns_to_a_v7_database() {
    let dir = tempfile();
    let path = dir.join("columns.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        conn.execute_batch(
            "ALTER TABLE blocks DROP COLUMN columns;
             PRAGMA user_version = 7;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages (id, title, parent, ord, favorite, expanded)
             VALUES (1, 'Old', NULL, 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blocks (id, page, kind, text, checked, color, bg, folded, attachment, img_percent)
             VALUES (10, 1, 'paragraph', 'from v7', 0, '', '', 0, NULL, 100)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO block_children (block, parent, ord) VALUES (10, NULL, 100)", [])
            .unwrap();
        drop(conn);

        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), 7);
        // control: the column must really be gone, or the test passes without
        // the v8 step ever running
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'columns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 0, "the rolled-back schema has no columns column");

        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), migrations::CURRENT_VERSION);
        let columns: i64 =
            conn.query_row("SELECT columns FROM blocks WHERE id = 10", [], |r| r.get(0)).unwrap();
        assert_eq!(columns, 0, "a pre-table row is not a table");
        migrations::ensure_current(&mut conn).unwrap();
        migrations::check_schema(&conn).unwrap();
    }
    let state = SqliteRepository::open(&path).unwrap().load().unwrap();
    assert_eq!(state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap().columns, 0);
}

/// v9 is a conditional ALTER like the four before it, so it has to run against
/// a database that really lacks the column.
#[test]
fn the_v9_step_adds_lang_to_a_v8_database() {
    let dir = tempfile();
    let path = dir.join("lang.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        conn.execute_batch(
            "ALTER TABLE blocks DROP COLUMN lang;
             PRAGMA user_version = 8;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages (id, title, parent, ord, favorite, expanded)
             VALUES (1, 'Old', NULL, 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blocks (id, page, kind, text, checked, color, bg, folded, attachment, img_percent, columns)
             VALUES (10, 1, 'code', 'fn main() {}', 0, '', '', 0, NULL, 100, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO block_children (block, parent, ord) VALUES (10, NULL, 100)", [])
            .unwrap();
        drop(conn);

        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), 8);
        // control: the column must really be gone, or the test passes without
        // the v9 step ever running
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('blocks') WHERE name = 'lang'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 0, "the rolled-back schema has no lang column");

        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), migrations::CURRENT_VERSION);
        let lang: String =
            conn.query_row("SELECT lang FROM blocks WHERE id = 10", [], |r| r.get(0))
                .unwrap();
        assert_eq!(lang, "", "a code block from before the picker is uncoloured");
        migrations::ensure_current(&mut conn).unwrap();
        migrations::check_schema(&conn).unwrap();
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let state = repo.load().unwrap();
    assert_eq!(state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap().lang, Lang::Plain);
    // and the column carries a real language through the same file
    repo.apply(&[Change::BlockLangSet { id: BlockId(10), lang: Lang::Rust }])
        .unwrap();
    assert_eq!(
        repo.load().unwrap().blocks.iter().find(|b| b.id == BlockId(10)).unwrap().lang,
        Lang::Rust
    );
}

#[test]
fn the_v10_step_adds_the_page_look_to_a_v9_database() {
    let dir = tempfile();
    let path = dir.join("style.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        conn.execute_batch(
            "ALTER TABLE pages DROP COLUMN font;
             ALTER TABLE pages DROP COLUMN layout;
             PRAGMA user_version = 9;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages (id, title, parent, ord, favorite, expanded)
             VALUES (1, 'Old', NULL, 1, 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), 9);
        // control: both columns must really be gone, or the step never ran and
        // the defaults below prove nothing
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('pages')
                 WHERE name IN ('font', 'layout')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 0, "the rolled-back schema has neither column");

        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(
            migrations::user_version(&conn).unwrap(),
            migrations::CURRENT_VERSION
        );
        let (font, layout): (String, i64) =
            conn.query_row("SELECT font, layout FROM pages WHERE id = 1", [], |r| {
                Ok((r.get(0).unwrap(), r.get(1).unwrap()))
            })
            .unwrap();
        assert_eq!((font.as_str(), layout), ("", 0), "an old page looks the default");
        migrations::ensure_current(&mut conn).unwrap();
        migrations::check_schema(&conn).unwrap();
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let loaded = || {
        repo.load()
            .unwrap()
            .pages
            .into_iter()
            .find(|p| p.id == PageId(1))
            .unwrap()
    };
    assert_eq!(loaded().font, PageFont::Default);

    repo.apply(&[Change::PageFontSet {
        id: PageId(1),
        font: PageFont::Serif,
    }])
    .unwrap();
    assert_eq!(loaded().font, PageFont::Serif);

    // The two switches share one column, so each write says both bits; the
    // load must read back exactly the pair that was stored.
    repo.apply(&[Change::PageLayoutSet {
        id: PageId(1),
        full_width: true,
        small_text: false,
    }])
    .unwrap();
    let page = loaded();
    assert!(page.full_width && !page.small_text);
    repo.apply(&[Change::PageLayoutSet {
        id: PageId(1),
        full_width: true,
        small_text: true,
    }])
    .unwrap();
    let page = loaded();
    assert!(page.full_width && page.small_text);
    let bits: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("SELECT layout FROM pages WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bits, 3, "the two switches pack into 1 | 2");
    assert!(page.font == PageFont::Serif, "the layout write left the font alone");

    // An unreadable spelling is no font, not a page that cannot open.
    repo.apply(&[Change::PageFontSet {
        id: PageId(1),
        font: PageFont::Mono,
    }])
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE pages SET font = 'Georgia-ish' WHERE id = 1", [])
        .unwrap();
    drop(conn);
    assert_eq!(loaded().font, PageFont::Default);
}

#[test]
fn the_v11_step_adds_the_page_icon_to_a_v10_database() {
    let dir = tempfile();
    let path = dir.join("icon.db");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        conn.execute_batch(
            "ALTER TABLE pages DROP COLUMN icon;
             PRAGMA user_version = 10;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pages (id, title, parent, ord, favorite, expanded, font, layout)
             VALUES (1, 'Old', NULL, 1, 0, 0, '', 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let mut conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(migrations::user_version(&conn).unwrap(), 10);
        // control: the column really is gone, or the step never ran and the
        // empty default below proves nothing
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('pages') WHERE name = 'icon'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 0, "the rolled-back schema has no icon column");

        migrations::ensure_current(&mut conn).unwrap();
        assert_eq!(
            migrations::user_version(&conn).unwrap(),
            migrations::CURRENT_VERSION
        );
        let icon: String = conn
            .query_row("SELECT icon FROM pages WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(icon, "", "a page from before the picker has no icon");
        // the look the v10 step stored is still there: a new column must not
        // disturb the old ones
        let (font, layout): (String, i64) =
            conn.query_row("SELECT font, layout FROM pages WHERE id = 1", [], |r| {
                Ok((r.get(0).unwrap(), r.get(1).unwrap()))
            })
            .unwrap();
        assert_eq!((font.as_str(), layout), ("", 0));
        migrations::ensure_current(&mut conn).unwrap();
        migrations::check_schema(&conn).unwrap();
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let loaded = || {
        repo.load()
            .unwrap()
            .pages
            .into_iter()
            .find(|p| p.id == PageId(1))
            .unwrap()
    };
    assert_eq!(loaded().icon, "");

    // The emoji itself, not an index into the picker's grid.
    repo.apply(&[Change::PageIconSet {
        id: PageId(1),
        icon: "\u{1f680}".into(),
    }])
    .unwrap();
    assert_eq!(loaded().icon, "\u{1f680}");
    // Clearing is a write like any other, not a no-op that leaves the old one.
    repo.apply(&[Change::PageIconSet {
        id: PageId(1),
        icon: String::new(),
    }])
    .unwrap();
    assert_eq!(loaded().icon, "");
    // A write that touches only the icon leaves the look alone.
    repo.apply(&[Change::PageIconSet {
        id: PageId(1),
        icon: "\u{1f33f}".into(),
    }])
    .unwrap();
    repo.apply(&[Change::PageFontSet {
        id: PageId(1),
        font: PageFont::Serif,
    }])
    .unwrap();
    let page = loaded();
    assert_eq!(page.icon, "\u{1f33f}");
    assert_eq!(page.font, PageFont::Serif);
}

#[test]
fn a_page_created_with_a_look_keeps_it_through_the_insert_path() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.apply(&[Change::PageCreated(Page {
        id: PageId(3),
        title: "Written".into(),
        parent: None,
        order: OrderKey(1),
        favorite: false,
        expanded: false,
        font: PageFont::Mono,
        full_width: true,
        small_text: false,
        icon: "\u{1f6f0}".into(),
    })])
    .unwrap();
    let page = repo.load().unwrap().pages.remove(0);
    assert_eq!(
        (page.font, page.full_width, page.small_text),
        (PageFont::Mono, true, false),
        "the look is written with the page, not only updated later"
    );
    assert_eq!("\u{1f6f0}", page.icon, "and so is the icon");
}

#[test]
fn attachments_round_trip_and_a_dangling_reference_still_loads() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.apply(&[
        Change::PageCreated(page(1, "One", None, 1 << 32)),
        Change::AttachmentAdded(Attachment {
            id: AttachmentId(4),
            name: "Sunset photo.png".into(),
            file: "4.png".into(),
            thumb: "4.cache.png".into(),
            mime: "image/png".into(),
            bytes: 12345,
            width: 1280,
            height: 720,
        }),
        Change::BlockInserted(Block {
            kind: BlockKind::Image,
            attachment: Some(AttachmentId(4)),
            img_percent: 50,
            columns: 0,
            lang: Lang::Plain,
            ..block(10, 1, None, 100, "Sunset photo.png")
        }),
    ])
    .unwrap();

    let atts = repo.load_attachments().unwrap();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].name, "Sunset photo.png");
    assert_eq!((atts[0].width, atts[0].height), (1280, 720));

    let state = repo.load().unwrap();
    let pic = state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap();
    assert_eq!(pic.attachment, Some(AttachmentId(4)));
    assert_eq!(pic.img_percent, 50);

    // re-adding the same id is an upsert, not a duplicate or a failure
    repo.apply(&[Change::AttachmentAdded(Attachment { bytes: 999, ..atts[0].clone() })]).unwrap();
    let atts = repo.load_attachments().unwrap();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].bytes, 999);

    repo.apply(&[Change::BlockImageWidthSet { id: BlockId(10), percent: 100 }]).unwrap();
    let state = repo.load().unwrap();
    assert_eq!(state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap().img_percent, 100);
}

/// The reclaim's write arm: one row goes, the block that used to point at it
/// stays (SPEC §三十七, ADR-0037). Deleting an id that is not there is a no-op,
/// so a sweep is replayable.
#[test]
fn an_attachment_row_can_be_deleted_and_repeatedly() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.apply(&[
        Change::PageCreated(page(1, "One", None, 1 << 32)),
        Change::AttachmentAdded(Attachment {
            id: AttachmentId(4),
            name: "gone.png".into(),
            file: "4.png".into(),
            thumb: String::new(),
            mime: "image/png".into(),
            bytes: 10,
            width: 4,
            height: 4,
        }),
        Change::BlockInserted(Block {
            kind: BlockKind::Image,
            attachment: Some(AttachmentId(4)),
            ..block(10, 1, None, 100, "gone.png")
        }),
    ])
    .unwrap();
    assert_eq!(repo.load_attachments().unwrap().len(), 1);

    repo.apply(&[Change::AttachmentDeleted { id: AttachmentId(4) }]).unwrap();
    assert!(repo.load_attachments().unwrap().is_empty(), "the row is gone");
    let state = repo.load().unwrap();
    assert_eq!(
        state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap().attachment,
        Some(AttachmentId(4)),
        "and nothing cascaded: the dangling reference is the load path's problem, not this one"
    );

    repo.apply(&[Change::AttachmentDeleted { id: AttachmentId(4) }]).unwrap();
    repo.apply(&[Change::AttachmentDeleted { id: AttachmentId(99) }]).unwrap();
}

/// `blocks.attachment` deliberately carries no foreign key: a picture whose
/// file row is gone must render as a missing image, not fail the library.
#[test]
fn a_picture_whose_attachment_row_vanished_still_loads() {
    let dir = tempfile();
    let path = dir.join("dangling.db");
    {
        let repo = SqliteRepository::open(&path).unwrap();
        repo.apply(&[
            Change::PageCreated(page(1, "One", None, 1 << 32)),
            Change::AttachmentAdded(Attachment {
                id: AttachmentId(4),
                name: "photo.png".into(),
                file: "4.png".into(),
                thumb: String::new(),
                mime: "image/png".into(),
                bytes: 10,
                width: 4,
                height: 4,
            }),
            Change::BlockInserted(Block {
                kind: BlockKind::Image,
                attachment: Some(AttachmentId(4)),
                ..block(10, 1, None, 100, "photo.png")
            }),
        ])
        .unwrap();
    }
    // the row disappears behind Quire's back — no command plan emits
    // `AttachmentDeleted`, because undo must never throw bytes away; only the
    // settings-disk reclaim does, and only for a row nothing points at
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(conn.execute("DELETE FROM attachments", []).unwrap(), 1);
        drop(conn);
    }
    let repo = SqliteRepository::open(&path).unwrap();
    let state = repo.load().unwrap();
    let pic = state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap();
    assert_eq!(pic.attachment, Some(AttachmentId(4)), "the dangling reference survives");
    assert!(repo.load_attachments().unwrap().is_empty());
}

#[test]
fn fold_state_round_trips_and_a_missing_id_is_an_error() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.apply(&[
        Change::PageCreated(page(1, "One", None, 1 << 32)),
        Change::BlockInserted(Block {
            kind: BlockKind::Toggle,
            folded: true,
            ..block(10, 1, None, 100, "section")
        }),
        Change::BlockInserted(block(11, 1, Some(10), 110, "hidden child")),
    ])
    .unwrap();

    let state = repo.load().unwrap();
    assert!(state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap().folded);
    assert!(!state.blocks.iter().find(|b| b.id == BlockId(11)).unwrap().folded);

    repo.apply(&[Change::BlockFoldedSet { id: BlockId(11), folded: true }]).unwrap();
    let state = repo.load().unwrap();
    assert!(state.blocks.iter().find(|b| b.id == BlockId(11)).unwrap().folded);

    // unfolding writes false back; the flag is not a one-way door
    repo.apply(&[Change::BlockFoldedSet { id: BlockId(10), folded: false }]).unwrap();
    let state = repo.load().unwrap();
    assert!(!state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap().folded);

    let err = repo
        .apply(&[Change::BlockFoldedSet { id: BlockId(999), folded: true }])
        .unwrap_err();
    assert!(matches!(err, StorageError::Sql(_)), "got {err}");
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

    // Bounded retry: an orphan whose parent died before accepting must not
    // sit here forever. On Windows a live child keeps the test exe open, and
    // every later `cargo test` then fails to link it (LNK1104).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut stream = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => std::process::exit(0x7F),
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

/// A scratch database folder that deletes itself with the test.
///
/// These fixtures used to be a bare `PathBuf` and nothing removed them: over
/// many runs `%TEMP%` collected six hundred `quire-test-*` directories, each
/// holding a database nobody would open again.
fn tempfile() -> ScratchDir {
    ScratchDir::new("test")
}

// ── block colors + cross-page moves (schema v4, ADR-0023) ───────────

#[test]
fn block_colors_and_page_moves_round_trip() {
    let repo = SqliteRepository::in_memory().unwrap();
    repo.apply(&[
        Change::PageCreated(page(1, "One", None, 1 << 32)),
        Change::PageCreated(page(2, "Two", None, (1 << 32) + 2)),
        Change::BlockInserted(block(10, 1, None, 100, "colored")),
        Change::BlockInserted(block(11, 1, None, 120, "plain")),
    ])
    .unwrap();

    // text + background color land in the database...
    repo.apply(&[Change::BlockColorSet {
        id: BlockId(10),
        color: quire::core::ColorKind::Red,
        background: quire::core::ColorKind::Yellow,
    }])
    .unwrap();
    // ...and survive a full reload
    let state = repo.load().unwrap();
    let b = state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap();
    assert_eq!(b.color, quire::core::ColorKind::Red);
    assert_eq!(b.background, quire::core::ColorKind::Yellow);
    let b = state.blocks.iter().find(|b| b.id == BlockId(11)).unwrap();
    assert_eq!(b.color, quire::core::ColorKind::Default);

    // a cross-page move re-homes the row and its tree position
    repo.apply(&[Change::BlockMovedToPage {
        id: BlockId(10),
        page: PageId(2),
        parent: None,
        order: OrderKey(200),
    }])
    .unwrap();
    let state = repo.load().unwrap();
    let moved = state.blocks.iter().find(|b| b.id == BlockId(10)).unwrap();
    assert_eq!(moved.page, PageId(2));
    assert_eq!(moved.order, OrderKey(200));
    // the color rode along
    assert_eq!(moved.color, quire::core::ColorKind::Red);
    assert!(state.blocks.iter().all(|b| b.id != BlockId(10) || b.page == PageId(2)));
}

// ─── SPEC §三十九 Database (Track 3, D1) ─────────────────────────────────────
//
// Four migration steps (v12–v15), the reopen that has to come back field for
// field, and ADR-0063's record/page lifecycle. The module is nested so the
// helpers above stay where they are; everything here is ordinary public API —
// no test reaches inside the store.

mod database_layer {
    use super::*;
    use quire::core::database::{
        CellValue, Database, DatabaseId, Property, PropertyId, PropertyKind, Record, RecordId,
        RowRequest, RowWindow, View, ViewId, ViewLayout,
    };

    /// A page fixture: the module-level `page` above takes the tree position
    /// too, and every page these tests make is a top-level one whose only
    /// interesting field is its title — which is exactly what ADR-0063 says a
    /// record's title reads when the record owns the page.
    fn page(id: u64, title: &str) -> Page {
        super::page(id, title, None, 1 << 32)
    }

    /// A column of database 1, in the shape the store takes.
    pub(crate) fn column(id: u64, name: &str, kind: PropertyKind, ordinal: u64) -> Property {
        Property {
            id: PropertyId(id),
            db: DatabaseId(1),
            name: name.into(),
            kind,
            config: String::new(),
            ord: OrderKey(OrderKey::FIRST.0 + ordinal * 0x100),
        }
    }

    /// Open a scratch database, migrate it to the current version, then roll the
    /// *version number and schema* back to `version` — the only honest way to
    /// stand in front of one step: a real older file, not a stub.
    ///
    /// Only the tables the steps *after* `version` add are dropped, leaf-first
    /// (`db_value_items` references `db_records` and `db_properties`), so a v13
    /// fixture really does hold `databases` and `db_properties` and nothing
    /// later.
    pub(crate) fn migrated(name: &str) -> (ScratchDir, std::path::PathBuf) {
        let dir = ScratchDir::new(name);
        let path = dir.join("quire.db");
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        migrations::ensure_current(&mut conn).unwrap();
        drop(conn);
        (dir, path)
    }

    /// Drop what the steps after `version` add and put the version number back,
    /// leaving a real older file behind. `pub(crate)` — and this module reached
    /// as `crate::database_layer` — because a later step's migration test is
    /// exactly this shape: a real older file, a version number, and the rows it
    /// already had.
    pub(crate) fn roll_back(path: &Path, version: i32) {
        let conn = rusqlite::Connection::open(path).unwrap();
        let added_later: &[&str] = match version {
            11 => &[
                "db_views",
                "db_value_items",
                "db_values",
                "db_records",
                "db_properties",
                "databases",
            ],
            12 => &[
                "db_views",
                "db_value_items",
                "db_values",
                "db_records",
                "db_properties",
            ],
            13 => &["db_views", "db_value_items", "db_values", "db_records"],
            14 => &["db_views"],
            // Step 15 added `db_views`, step 16 added nothing but two indexes
            // — so a v15 fixture keeps every table and drops only the indexes
            // below.
            15 => &[],
            other => panic!("no rollback fixture for v{other}"),
        };
        for table in added_later {
            conn.execute(&format!("DROP TABLE IF EXISTS {table}"), [])
                .unwrap();
        }
        // Step 16 is an index-only step, so standing in front of it means the
        // indexes are gone, not the tables. (`DROP TABLE IF EXISTS <index>` is
        // a silent no-op in SQLite, which is exactly the trap this spells out.)
        if version < 16 {
            for index in ["idx_marks_reference", "idx_blocks_page_ref"] {
                conn.execute(&format!("DROP INDEX IF EXISTS {index}"), [])
                    .unwrap();
            }
        }
        conn.pragma_update(None, "user_version", version).unwrap();
    }

    /// `(id, name)` of every database row, read with plain SQL.
    pub(crate) fn raw_databases(path: &Path) -> Vec<(i64, String)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut stmt = conn.prepare("SELECT id, name FROM databases ORDER BY id").unwrap();
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    }

    /// How many rows one table holds, with plain SQL.
    pub(crate) fn raw_rows(path: &Path, table: &str) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn the_v12_step_adds_the_databases_table_to_a_v11_database() {
        let (_dir, path) = migrated("d1-v12");
        {
            // what a v11 library holds: pages, blocks, and nothing about
            // databases anywhere
            let repo = SqliteRepository::open(&path).unwrap();
            repo.apply(&[Change::PageCreated(page(1, "Written before the step"))])
                .unwrap();
        }
        roll_back(&path, 11);

        let repo = SqliteRepository::open(&path).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            migrations::user_version(&conn).unwrap(),
            migrations::CURRENT_VERSION
        );
        migrations::check_schema(&conn).unwrap();
        // The point of the step: the table is there and it is empty.
        assert_eq!(raw_rows(&path, "databases"), 0);
        drop(conn);

        // …and the library it upgraded is exactly what it was.
        let state = repo.load().unwrap();
        assert_eq!(state.pages.len(), 1);
        assert_eq!(state.pages[0].title, "Written before the step");
        let catalog = repo.load_databases().unwrap();
        assert!(
            catalog.databases.is_empty() && catalog.properties.is_empty() && catalog.views.is_empty(),
            "a v11 library has no database, and that is a valid state"
        );
    }

    #[test]
    fn the_v13_step_adds_the_property_columns_to_a_v12_database() {
        let (_dir, path) = migrated("d1-v13");
        {
            let repo = SqliteRepository::open(&path).unwrap();
            repo.apply(&[Change::DatabaseCreated(Database::new(
                DatabaseId(7),
                "Written before the step",
            ))])
            .unwrap();
        }
        roll_back(&path, 12);

        let repo = SqliteRepository::open(&path).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            migrations::user_version(&conn).unwrap(),
            migrations::CURRENT_VERSION
        );
        migrations::check_schema(&conn).unwrap();
        drop(conn);

        // The row that existed at v12 reads back unchanged — that is what the
        // upgrade may not touch.
        assert_eq!(
            raw_databases(&path),
            vec![(7, "Written before the step".to_string())]
        );
        // …and the new table takes rows that point at it, with the uniqueness
        // ADR-0061 rests on doing its job.
        let db = Database::new(DatabaseId(7), "Written before the step");
        repo.apply(&[Change::PropertyAdded(db.title_property(PropertyId(1)))])
            .unwrap();
        let twice = repo.apply(&[Change::PropertyAdded(Property {
            id: PropertyId(2),
            ..db.title_property(PropertyId(2))
        })]);
        assert!(
            matches!(twice, Err(StorageError::Sql(_))),
            "two columns called Name was refused, got {twice:?}"
        );
        let catalog = repo.load_databases().unwrap();
        assert_eq!(catalog.database(DatabaseId(7)).unwrap().name, "Written before the step");
        assert_eq!(catalog.properties_of(DatabaseId(7)).count(), 1);
        assert!(catalog.title_property(DatabaseId(7)).unwrap().kind.is_title());
    }

    #[test]
    fn the_v14_step_adds_records_and_their_values_to_a_v13_database() {
        let (_dir, path) = migrated("d1-v14");
        {
            let repo = SqliteRepository::open(&path).unwrap();
            let db = Database::new(DatabaseId(7), "Written before the step");
            repo.apply(&[
                Change::DatabaseCreated(db.clone()),
                Change::PropertyAdded(db.title_property(PropertyId(1))),
            ])
            .unwrap();
        }
        roll_back(&path, 13);

        let repo = SqliteRepository::open(&path).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            migrations::user_version(&conn).unwrap(),
            migrations::CURRENT_VERSION
        );
        migrations::check_schema(&conn).unwrap();
        drop(conn);

        // The three tables the step added, empty…
        for table in ["db_records", "db_values", "db_value_items"] {
            assert_eq!(raw_rows(&path, table), 0, "{table} arrived empty");
        }
        // …and the database and its column from v13 read back unchanged.
        let catalog = repo.load_databases().unwrap();
        let property = catalog.title_property(DatabaseId(7)).unwrap();
        assert_eq!(
            (property.id, property.name.as_str(), property.ord),
            (PropertyId(1), "Name", OrderKey::FIRST)
        );
        assert_eq!(repo.record_count(DatabaseId(7)).unwrap(), 0, "no rows at v13");

        // The new tables take a row with its title, through the ordinary path.
        let record = RecordId(1);
        repo.apply(&[
            Change::RecordCreated(Record::bare(
                record,
                DatabaseId(7),
                OrderKey(OrderKey::FIRST.0 + 0x100),
            )),
            Change::CellSet {
                record,
                property: PropertyId(1),
                value: CellValue::Text("written after the step".into()),
            },
        ])
        .unwrap();
        assert_eq!(
            repo.record_title(record).unwrap().as_deref(),
            Some("written after the step")
        );
        assert_eq!(raw_rows(&path, "db_values"), 1);
    }

    #[test]
    fn the_v15_step_adds_views_to_a_v14_database() {
        let (_dir, path) = migrated("d1-v15");
        {
            let repo = SqliteRepository::open(&path).unwrap();
            let db = Database::new(DatabaseId(7), "Written before the step");
            repo.apply(&[
                Change::DatabaseCreated(db.clone()),
                Change::PropertyAdded(db.title_property(PropertyId(1))),
                Change::RecordCreated(Record::bare(RecordId(1), db.id, OrderKey::FIRST)),
                Change::CellSet {
                    record: RecordId(1),
                    property: PropertyId(1),
                    value: CellValue::Text("kept across the step".into()),
                },
            ])
            .unwrap();
        }
        roll_back(&path, 14);

        let repo = SqliteRepository::open(&path).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            migrations::user_version(&conn).unwrap(),
            migrations::CURRENT_VERSION
        );
        migrations::check_schema(&conn).unwrap();
        drop(conn);

        assert_eq!(raw_rows(&path, "db_views"), 0, "the table arrived empty");
        // Every row written before the step is exactly where it was.
        assert_eq!(raw_rows(&path, "db_records"), 1);
        assert_eq!(raw_rows(&path, "db_values"), 1);
        assert_eq!(
            repo.record_title(RecordId(1)).unwrap().as_deref(),
            Some("kept across the step")
        );
        // A v14 database has no views, which is the state D3's switcher has to
        // survive — and the new table takes one.
        assert_eq!(repo.load_databases().unwrap().views.len(), 0);
        let db = Database::new(DatabaseId(7), "Written before the step");
        repo.apply(&[Change::ViewAdded(db.first_view(ViewId(1)))])
            .unwrap();
        let catalog = repo.load_databases().unwrap();
        assert_eq!(catalog.views_of(DatabaseId(7)).count(), 1);
        assert_eq!(catalog.views_of(DatabaseId(7)).next().unwrap().layout, ViewLayout::Table);
    }

    /// The one D1 owes by name: a database read back from disk, field for
    /// field, after the file was closed and opened again.
    #[test]
    fn a_database_comes_back_field_for_field_after_a_reopen() {
        let dir = ScratchDir::new("d1-reopen");
        let path = dir.join("quire.db");
        let (db, records) = {
            let repo = SqliteRepository::open(&path).unwrap();
            let db = Database::new(DatabaseId(1), "Tasks and chores");
            let mut select = column(3, "Status", PropertyKind::Status, 2);
            select.config = r#"{"options":[{"id":7,"name":"Done","color":"green"}]}"#.into();
            repo.apply(&[
                Change::DatabaseCreated(db.clone()),
                Change::PropertyAdded(db.title_property(PropertyId(1))),
                Change::PropertyAdded(column(2, "Estimate", PropertyKind::Number, 1)),
                Change::PropertyAdded(select),
                Change::PropertyAdded(column(4, "Tags", PropertyKind::MultiSelect, 3)),
                Change::ViewAdded(db.first_view(ViewId(1))),
                Change::ViewAdded(View {
                    id: ViewId(2),
                    db: db.id,
                    name: "Board".into(),
                    layout: ViewLayout::Board,
                    definition: r#"{"v":1,"groups":[{"property":3}]}"#.into(),
                    ord: OrderKey(OrderKey::FIRST.0 + 0x100),
                }),
                Change::PageCreated(page(50, "Face of a row")),
            ])
            .unwrap();

            // Three rows: one bare, one that owns a page, one empty.
            let records = [RecordId(1), RecordId(2), RecordId(3)];
            repo.apply(&[
                Change::RecordCreated(Record::bare(
                    records[0],
                    db.id,
                    OrderKey(OrderKey::FIRST.0 + 0x100),
                )),
                Change::RecordCreated(Record::bare(
                    records[1],
                    db.id,
                    OrderKey(OrderKey::FIRST.0 + 0x200),
                )),
                Change::RecordCreated(Record {
                    id: records[2],
                    db: db.id,
                    page: Some(PageId(50)),
                    ord: OrderKey(OrderKey::FIRST.0 + 0x300),
                }),
            ])
            .unwrap();
            repo.apply(&[
                Change::CellSet {
                    record: records[0],
                    property: PropertyId(1),
                    value: CellValue::Text("Bare row".into()),
                },
                Change::CellSet {
                    record: records[0],
                    property: PropertyId(2),
                    value: CellValue::Number(2.5),
                },
                Change::CellSet {
                    record: records[0],
                    property: PropertyId(3),
                    value: CellValue::Text("7".into()),
                },
                Change::CellSet {
                    record: records[0],
                    property: PropertyId(4),
                    value: CellValue::Items(vec!["7".into(), "9".into()]),
                },
                Change::CellSet {
                    record: records[1],
                    property: PropertyId(1),
                    value: CellValue::Text("Second row".into()),
                },
                Change::CellSet {
                    record: records[1],
                    property: PropertyId(2),
                    value: CellValue::Number(-1.0),
                },
            ])
            .unwrap();
            (db, records)
        };

        // Close, open, read: the store is the only thing that remembers.
        let reopened = SqliteRepository::open(&path).unwrap();
        let catalog = reopened.load_databases().unwrap();
        assert_eq!(catalog.databases, vec![db.clone()], "the entity came back");

        let properties: Vec<_> = catalog.properties_of(db.id).cloned().collect();
        assert_eq!(properties.len(), 4);
        assert_eq!(
            properties.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![PropertyId(1), PropertyId(2), PropertyId(3), PropertyId(4)],
            "columns come back in ord order"
        );
        assert_eq!(properties[1].name, "Estimate");
        assert_eq!(properties[1].kind, PropertyKind::Number);
        assert_eq!(
            properties[2].config,
            r#"{"options":[{"id":7,"name":"Done","color":"green"}]}"#
        );
        assert_eq!(properties[3].kind, PropertyKind::MultiSelect);

        let views: Vec<_> = catalog.views_of(db.id).cloned().collect();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].name, "Table");
        assert_eq!(views[0].layout, ViewLayout::Table);
        assert_eq!(views[0].definition, "");
        assert_eq!(views[1].name, "Board");
        assert_eq!(views[1].layout, ViewLayout::Board);
        assert_eq!(views[1].definition, r#"{"v":1,"groups":[{"property":3}]}"#);

        // The rows, with their order, their pages and their cells.
        for (index, record) in records.iter().enumerate() {
            let loaded = reopened.record(*record).unwrap().unwrap();
            assert_eq!(loaded.db, db.id);
            assert_eq!(loaded.ord, OrderKey(OrderKey::FIRST.0 + 0x100 * (index as u64 + 1)));
            assert_eq!(
                loaded.page,
                if index == 2 { Some(PageId(50)) } else { None }
            );
        }
        assert_eq!(
            reopened.cell(records[0], PropertyId(2)).unwrap(),
            CellValue::Number(2.5)
        );
        assert_eq!(
            reopened.cell(records[0], PropertyId(4)).unwrap(),
            CellValue::Items(vec!["7".into(), "9".into()])
        );
        assert_eq!(
            reopened.cell(records[1], PropertyId(2)).unwrap(),
            CellValue::Number(-1.0),
            "a negative number is not a parsing accident"
        );
        assert_eq!(
            reopened.cell(records[2], PropertyId(4)).unwrap(),
            CellValue::Empty,
            "a cell nobody wrote stays absent"
        );
        assert_eq!(reopened.record_title(records[0]).unwrap().as_deref(), Some("Bare row"));
        assert_eq!(
            reopened.record_title(records[2]).unwrap().as_deref(),
            Some("Face of a row"),
            "a page-backed row's title is the page's"
        );

        // And the window read works on the file that came back.
        let columns: Vec<Property> = vec![
            catalog.title_property(db.id).cloned().unwrap(),
            properties[1].clone(),
        ];
        let req = RowRequest {
            db: db.id,
            title: PropertyId(1),
            columns: &columns,
        };
        assert_eq!(reopened.record_count(db.id).unwrap(), 3);
        let rows = reopened.window_rows(&req, RowWindow { start: 0, end: 3 }).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "Bare row");
        assert_eq!(rows[0].cells, vec!["Bare row".to_string(), "2.5".to_string()]);
        assert_eq!(rows[2].title, "Face of a row");
        assert_eq!(rows[2].cells, vec!["Face of a row".to_string(), "".to_string()]);
    }

    /// ADR-0063's two delete paths. They arrive from different places — the
    /// view's row menu and the sidebar — and they have to end in the same
    /// state: no row whose page is gone, and no page lost while its row lives.
    #[test]
    fn a_row_dies_with_the_page_it_owns_and_both_delete_paths_end_there() {
        let dir = ScratchDir::new("d1-lifecycle");
        let path = dir.join("quire.db");
        let repo = SqliteRepository::open(&path).unwrap();
        let db = Database::new(DatabaseId(1), "Tasks");
        let records = [RecordId(1), RecordId(2), RecordId(3)];
        repo.apply(&[
            Change::DatabaseCreated(db.clone()),
            Change::PropertyAdded(db.title_property(PropertyId(1))),
            Change::PropertyAdded(column(4, "Tags", PropertyKind::MultiSelect, 1)),
            Change::ViewAdded(db.first_view(ViewId(1))),
            Change::PageCreated(page(50, "Owes its row")),
            Change::PageCreated(page(51, "Owes its row too")),
        ])
        .unwrap();
        repo.apply(&[
            Change::RecordCreated(Record::bare(records[0], db.id, OrderKey::FIRST)),
            Change::RecordCreated(Record {
                id: records[1],
                db: db.id,
                page: Some(PageId(50)),
                ord: OrderKey(OrderKey::FIRST.0 + 0x100),
            }),
            Change::RecordCreated(Record {
                id: records[2],
                db: db.id,
                page: Some(PageId(51)),
                ord: OrderKey(OrderKey::FIRST.0 + 0x200),
            }),
            Change::CellSet {
                record: records[0],
                property: PropertyId(1),
                value: CellValue::Text("Bare".into()),
            },
            Change::CellSet {
                record: records[1],
                property: PropertyId(4),
                value: CellValue::Items(vec!["9".into()]),
            },
        ])
        .unwrap();

        // Path A — the row menu: the record and the page it owns, in one batch.
        // One `apply` is one transaction, which is what makes this one undo
        // step; the page is in the batch because the record owns it.
        repo.apply(&[
            Change::RecordDeleted { id: records[1] },
            Change::PageDeleted { id: PageId(50) },
        ])
        .unwrap();

        // Path B — the sidebar: the page alone. Nobody names the record, so the
        // `ON DELETE CASCADE` is the backstop that keeps the invariant true.
        repo.apply(&[Change::PageDeleted { id: PageId(51) }]).unwrap();

        // Both paths end in the same state.
        assert_eq!(repo.record(records[1]).unwrap(), None);
        assert_eq!(repo.record(records[2]).unwrap(), None);
        assert_eq!(repo.record_title(records[1]).unwrap(), None);
        let state = repo.load().unwrap();
        assert!(state.pages.is_empty(), "both pages are gone");
        assert_eq!(repo.record_count(db.id).unwrap(), 1, "the bare row is not");
        assert_eq!(repo.record(records[0]).unwrap().unwrap().page, None);
        assert_eq!(
            repo.record_title(records[0]).unwrap().as_deref(),
            Some("Bare"),
            "and it still has its title — deleting a row is not deleting a column"
        );
        // The invariant, stated as a read: no row points at a page that is not
        // there. That is the property, not the mechanism.
        let live: Vec<u64> = state.pages.iter().map(|p| p.id.as_u64()).collect();
        let rows = repo
            .window_rows(
                &RowRequest {
                    db: db.id,
                    title: PropertyId(1),
                    columns: &[],
                },
                RowWindow { start: 0, end: 31 },
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(live.is_empty() || rows.iter().all(|r| live.contains(&r.record)));

        // Undo is the inverse batch: the same rows come back, including the
        // page, because the delete captured them (ADR-0063).
        repo.apply(&[
            Change::PageCreated(page(50, "Owes its row")),
            Change::RecordCreated(Record {
                id: records[1],
                db: db.id,
                page: Some(PageId(50)),
                ord: OrderKey(OrderKey::FIRST.0 + 0x100),
            }),
            Change::CellSet {
                record: records[1],
                property: PropertyId(4),
                value: CellValue::Items(vec!["9".into()]),
            },
        ])
        .unwrap();
        assert_eq!(
            repo.record(records[1]).unwrap().unwrap().page,
            Some(PageId(50))
        );
        assert_eq!(
            repo.cell(records[1], PropertyId(4)).unwrap(),
            CellValue::Items(vec!["9".into()])
        );
        assert_eq!(repo.record_count(db.id).unwrap(), 2);
    }

    /// The bulk path (checkpoint / repair) replaces the *document*, and the
    /// database layer is not part of that state — but `DELETE FROM pages` still
    /// cascades through `db_records.page`. ADR-0066: the layer survives, except
    /// where it hangs off a page the incoming state dropped.
    #[test]
    fn the_bulk_path_keeps_the_database_layer_except_what_hangs_off_a_dropped_page() {
        let dir = ScratchDir::new("d1-bulk");
        let path = dir.join("quire.db");
        let repo = SqliteRepository::open(&path).unwrap();
        let db = Database::new(DatabaseId(1), "Tasks");
        let records = [RecordId(1), RecordId(2)];
        repo.apply(&[
            Change::DatabaseCreated(db.clone()),
            Change::PropertyAdded(db.title_property(PropertyId(1))),
            Change::PropertyAdded(column(2, "Notes", PropertyKind::Text, 1)),
            Change::ViewAdded(db.first_view(ViewId(1))),
            Change::PageCreated(page(50, "Kept by the state")),
            Change::PageCreated(page(51, "Dropped by the state")),
        ])
        .unwrap();
        repo.apply(&[
            Change::RecordCreated(Record {
                id: records[0],
                db: db.id,
                page: Some(PageId(50)),
                ord: OrderKey::FIRST,
            }),
            Change::RecordCreated(Record {
                id: records[1],
                db: db.id,
                page: Some(PageId(51)),
                ord: OrderKey(OrderKey::FIRST.0 + 0x100),
            }),
            // Both rows are page-backed, so their *titles* are the pages'
            // (ADR-0063); what the record itself carries is the other column.
            Change::CellSet {
                record: records[0],
                property: PropertyId(2),
                value: CellValue::Text("survives".into()),
            },
            Change::CellSet {
                record: records[1],
                property: PropertyId(2),
                value: CellValue::Text("goes with its page".into()),
            },
        ])
        .unwrap();

        // The state that keeps page 50 and knows nothing about page 51.
        let kept = PersistedState {
            pages: vec![page(50, "Kept by the state")],
            blocks: vec![],
            meta: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        repo.replace_all(&kept).unwrap();

        let catalog = repo.load_databases().unwrap();
        assert_eq!(catalog.databases.len(), 1, "the entity survived the bulk path");
        assert_eq!(catalog.properties.len(), 2, "and its columns did");
        assert_eq!(catalog.views.len(), 1);
        assert_eq!(repo.load().unwrap().pages.len(), 1);
        assert_eq!(
            repo.record(records[0]).unwrap().unwrap().page,
            Some(PageId(50)),
            "the row whose page the state kept is still the face of it"
        );
        assert_eq!(
            repo.record_title(records[0]).unwrap().as_deref(),
            Some("Kept by the state"),
            "the page the state handed back is still the row's title"
        );
        assert_eq!(
            repo.cell(records[0], PropertyId(2)).unwrap(),
            CellValue::Text("survives".into())
        );
        assert_eq!(
            repo.record(records[1]).unwrap(),
            None,
            "the row whose page the state dropped went with it"
        );
        assert_eq!(
            repo.cell(records[1], PropertyId(2)).unwrap(),
            CellValue::Empty,
            "and its values went too, rather than hanging off nothing"
        );
        assert_eq!(repo.record_count(db.id).unwrap(), 1);

        // A bulk path that hands back the same pages is a no-op for this layer.
        let both = PersistedState {
            pages: vec![page(50, "Kept by the state"), page(51, "Kept this time")],
            blocks: vec![],
            meta: BTreeMap::new(),
            settings: BTreeMap::new(),
        };
        repo.apply(&[Change::PageCreated(page(51, "Kept this time"))]).unwrap();
        repo.replace_all(&both).unwrap();
        assert_eq!(repo.record_count(db.id).unwrap(), 1);
        assert_eq!(
            repo.cell(records[0], PropertyId(2)).unwrap(),
            CellValue::Text("survives".into())
        );
        assert_eq!(
            repo.record_title(records[0]).unwrap().as_deref(),
            Some("Kept by the state")
        );
    }
}
