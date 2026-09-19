// M7 search acceptance tests (SPEC §二十): the FTS5 index must track every
// change batch, drop what was deleted, survive migration from a v1 database,
// match Chinese, and hand the service layer ranked per-page hits.

use std::collections::BTreeMap;
use std::sync::Arc;

use quire::core::persistence::{Change, Repository};
use quire::core::types::{Block, BlockId, BlockKind, OrderKey, Page, PageId, PersistedState};
use quire::services::search_service::SearchService;
use quire::storage::search_index::SearchRequest;
use quire::storage::SqliteRepository;

fn page(id: u64, title: &str) -> Page {
    Page {
        id: PageId(id),
        title: title.into(),
        parent: None,
        order: OrderKey(id * 10),
        favorite: false,
        expanded: false,
    }
}

fn block(id: u64, page_id: u64, text: &str) -> Block {
    Block {
        id: BlockId(id),
        page: PageId(page_id),
        parent: None,
        order: OrderKey(id * 10),
        kind: BlockKind::Paragraph,
        text: text.into(),
        checked: false,
        marks: Vec::new(),
        color: quire::core::ColorKind::Default,
        background: quire::core::ColorKind::Default,
    }
}

fn repo() -> Arc<SqliteRepository> {
    Arc::new(SqliteRepository::in_memory().expect("open in-memory repository"))
}

/// Pages 1 and 2, one Chinese and one English block each.
fn seed(repo: &SqliteRepository) {
    repo.apply(&[
        Change::PageCreated(page(1, "Getting Started 起步")),
        Change::PageCreated(page(2, "Design notes")),
        Change::BlockInserted(block(10, 1, "写作与中文测试：字体回退与行高")),
        Change::BlockInserted(block(11, 1, "The quick brown fox jumps")),
        Change::BlockInserted(block(20, 2, "the renderer planning for this quarter")),
    ])
    .unwrap();
}

fn hits(repo: &Arc<SqliteRepository>, query: &str) -> Vec<String> {
    SearchService::new(repo.clone())
        .query(query)
        .unwrap()
        .into_iter()
        .map(|h| h.title)
        .collect()
}

// ── index maintenance ───────────────────────────────────────────────

#[test]
fn inserted_content_is_searchable_immediately() {
    let repo = repo();
    seed(&repo);
    assert_eq!(hits(&repo, "renderer"), ["Design notes"]);
    assert_eq!(hits(&repo, "jumps"), ["Getting Started 起步"]);
    assert_eq!(hits(&repo, "Getting"), ["Getting Started 起步"]);
}

#[test]
fn a_text_edit_moves_the_index_with_it() {
    let repo = repo();
    seed(&repo);
    repo.apply(&[Change::BlockTextSet {
        id: BlockId(11),
        text: "The slow green turtle sleeps".into(),
    }])
    .unwrap();
    assert_eq!(hits(&repo, "turtle"), ["Getting Started 起步"]);
    assert!(hits(&repo, "quick").is_empty(), "stale text still matches");
    assert!(hits(&repo, "brown").is_empty(), "stale text still matches");
}

#[test]
fn emptying_a_block_unindexes_it() {
    let repo = repo();
    seed(&repo);
    repo.apply(&[Change::BlockTextSet {
        id: BlockId(20),
        text: String::new(),
    }])
    .unwrap();
    assert!(hits(&repo, "planning").is_empty());
}

#[test]
fn deleting_a_block_or_page_stops_matching() {
    let repo = repo();
    seed(&repo);
    repo.apply(&[Change::BlockDeleted { id: BlockId(20) }]).unwrap();
    assert!(hits(&repo, "renderer").is_empty(), "deleted block matches");
    assert_eq!(hits(&repo, "quick"), ["Getting Started 起步"]);

    repo.apply(&[Change::PageDeleted { id: PageId(1) }]).unwrap();
    assert!(hits(&repo, "quick").is_empty(), "deleted page's blocks match");
    assert!(
        hits(&repo, "起步").is_empty(),
        "deleted page's title still matches"
    );
}

#[test]
fn deleting_a_parent_prunes_its_block_subtree() {
    let repo = repo();
    repo.apply(&[
        Change::PageCreated(page(1, "P")),
        Change::BlockInserted(block(10, 1, "grandparent text")),
        Change::BlockInserted(block(
            11,
            1,
            "child text with a unique word: salamander",
        )),
        Change::BlockInserted(block(
            12,
            1,
            "grandchild text with numbskull inside",
        )),
    ])
    .unwrap();
    repo.apply(&[Change::BlockMoved {
        id: BlockId(11),
        parent: Some(BlockId(10)),
        order: OrderKey(5),
    }])
    .unwrap();
    repo.apply(&[Change::BlockMoved {
        id: BlockId(12),
        parent: Some(BlockId(11)),
        order: OrderKey(5),
    }])
    .unwrap();
    assert_eq!(hits(&repo, "numbskull"), ["P"]);
    // FK cascade removes 11 and 12 from `blocks`; the sweep must follow
    repo.apply(&[Change::BlockDeleted { id: BlockId(10) }]).unwrap();
    assert!(hits(&repo, "salamander").is_empty(), "cascade left a hit");
    assert!(hits(&repo, "numbskull").is_empty(), "cascade left a hit");
    assert_eq!(hits(&repo, "grandparent"), Vec::<String>::new());
}

#[test]
fn renaming_a_page_drops_the_old_title() {
    let repo = repo();
    seed(&repo);
    assert_eq!(hits(&repo, "起步"), ["Getting Started 起步"]);
    repo.apply(&[Change::PageTitleSet {
        id: PageId(1),
        title: "Onboarding".into(),
    }])
    .unwrap();
    assert!(hits(&repo, "起步").is_empty(), "old title still matches");
    assert!(hits(&repo, "Getting").is_empty(), "old title still matches");
    assert_eq!(hits(&repo, "Onboarding"), ["Onboarding"]);
    // the block text survives the rename
    assert_eq!(hits(&repo, "quick"), ["Onboarding"]);
}

#[test]
fn replace_all_rebuilds_the_mirror() {
    let repo = repo();
    seed(&repo);
    let mut settings = BTreeMap::new();
    settings.insert("dark".to_string(), "true".into());
    repo.replace_all(&PersistedState {
        pages: vec![page(50, "Only page")],
        blocks: vec![block(500, 50, "only content remains")],
        meta: BTreeMap::new(),
        settings,
    })
    .unwrap();
    assert!(hits(&repo, "quick").is_empty(), "bulk replace left a hit");
    assert!(hits(&repo, "起步").is_empty(), "bulk replace left a hit");
    assert_eq!(hits(&repo, "remains"), ["Only page"]);
}

// ── query language ──────────────────────────────────────────────────

#[test]
fn chinese_queries_match_through_segmentation() {
    let repo = repo();
    seed(&repo);
    assert_eq!(hits(&repo, "中文"), ["Getting Started 起步"]);
    assert_eq!(hits(&repo, "字体回退"), ["Getting Started 起步"]);
    // the phrase must stay adjacent: first and last characters of the text
    assert!(hits(&repo, "写高").is_empty(), "characters matched out of order");
    assert_eq!(hits(&repo, "起步"), ["Getting Started 起步"]);
}

#[test]
fn mixed_language_query_matches_both_halves() {
    let repo = repo();
    repo.apply(&[
        Change::PageCreated(page(1, "Mix")),
        Change::BlockInserted(block(10, 1, "Slint 渲染 backend")),
    ])
    .unwrap();
    assert_eq!(hits(&repo, "渲染"), ["Mix"]);
    assert_eq!(hits(&repo, "backend"), ["Mix"]);
    // one word spanning the boundary exists only as typed text, so the
    // segmented phrase query still finds it ("Slint 渲染" is adjacent)
    assert_eq!(hits(&repo, "Slint 渲染"), ["Mix"]);
}

#[test]
fn a_partial_word_matches_by_prefix() {
    let repo = repo();
    seed(&repo);
    assert_eq!(hits(&repo, "render"), ["Design notes"]);
    assert_eq!(hits(&repo, "quic"), ["Getting Started 起步"]);
    assert_eq!(hits(&repo, "bro"), ["Getting Started 起步"]);
    // prefix matching is one-directional: a longer query never matches a
    // shorter stored token
    assert!(hits(&repo, "quickly").is_empty());
}

#[test]
fn punctuation_and_blank_queries_match_nothing() {
    let repo = repo();
    seed(&repo);
    for q in ["", "   ", "——", "：：", "***"] {
        assert!(
            SearchService::new(repo.clone()).query(q).unwrap().is_empty(),
            "query {q:?} produced hits"
        );
    }
}

#[test]
fn raw_match_carries_the_stored_text_and_page_title() {
    let repo = repo();
    seed(&repo);
    let found = repo
        .search(&SearchRequest::new("fox"))
        .unwrap()
        .into_iter()
        .next()
        .expect("one match");
    assert_eq!(found.page, PageId(1));
    assert_eq!(found.block, Some(BlockId(11)));
    assert_eq!(found.text, "The quick brown fox jumps");
    assert_eq!(found.page_title, "Getting Started 起步");
}

// ── service layer ───────────────────────────────────────────────────

#[test]
fn hits_aggregate_per_page_with_a_block_to_open() {
    let repo = repo();
    seed(&repo);
    let service = SearchService::new(repo.clone());
    let all = service.query("the").unwrap();
    assert_eq!(all.len(), 2, "one hit per page expected: {all:?}");
    for hit in &all {
        assert!(hit.block.is_some() || hit.title.contains("the"));
    }
    // in-page search ignores the other page entirely
    let only = service.query_in_page("the", PageId(1)).unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].page, PageId(1));
    let none = service.query_in_page("renderer", PageId(1)).unwrap();
    assert!(none.is_empty());
}

#[test]
fn snippets_are_short_and_hold_the_term() {
    let repo = repo();
    repo.apply(&[
        Change::PageCreated(page(1, "Long")),
        Change::BlockInserted(block(
            10,
            1,
            &format!("{} needle {}", "filler ".repeat(40), "尾"),
        )),
    ])
    .unwrap();
    let hit = SearchService::new(repo.clone())
        .query("needle")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(hit.snippet.contains("needle"), "{}", hit.snippet);
    assert!(hit.snippet.chars().count() <= 82, "{}", hit.snippet);
    assert!(hit.snippet.starts_with('…'), "{}", hit.snippet);
}

#[test]
fn async_search_does_not_block_the_caller() {
    let repo = repo();
    seed(&repo);
    let service = SearchService::new(repo);
    let pending = service.search_async(SearchRequest::new("中文"));
    // The answer may already be there or still be on the way; either way the
    // caller never waits for SQLite, and the handle resolves eventually.
    let first = pending.poll();
    if let Some(result) = first {
        assert_eq!(result.unwrap()[0].title, "Getting Started 起步");
        return;
    }
    // re-poll: `poll` borrows, so the handle is still usable
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(result) = pending.poll() {
            assert_eq!(result.unwrap()[0].title, "Getting Started 起步");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "worker never answered");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn an_aborted_batch_leaves_the_index_untouched() {
    let repo = repo();
    seed(&repo);
    // the text update targets a missing id, so the whole transaction —
    // including the index writes before it — must roll back (ADR-0012)
    let bad = repo.apply(&[
        Change::BlockInserted(block(99, 2, "never committed at all")),
        Change::BlockTextSet {
            id: BlockId(40_404),
            text: "nope".into(),
        },
    ]);
    assert!(bad.is_err());
    assert!(hits(&repo, "never").is_empty(), "rolled-back write is indexed");
    assert_eq!(hits(&repo, "quick"), ["Getting Started 起步"]);
}

// ── migration ───────────────────────────────────────────────────────

#[test]
fn a_v1_database_is_backfilled_on_upgrade() {
    let dir = std::env::temp_dir().join(format!("quire-search-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("search.db");

    {
        let repo = SqliteRepository::open(&path).unwrap();
        seed(&repo);
    }
    // pretend the file predates M7: drop everything added after v1 and
    // claim version 1 (the marks table is v3, also dropped)
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE search_pages;
             DROP TABLE search_blocks;
             DROP TABLE marks;
             PRAGMA user_version = 1;",
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM blocks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
    let repo = Arc::new(SqliteRepository::open(&path).unwrap());
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            quire::storage::migrations::user_version(&conn).unwrap(),
            quire::storage::migrations::CURRENT_VERSION
        );
    }
    // data written before the upgrade is searchable again
    assert_eq!(hits(&repo, "中文"), ["Getting Started 起步"]);
    assert_eq!(hits(&repo, "quarter"), ["Design notes"]);
    // and maintenance still works on the rebuilt tables
    repo.apply(&[Change::BlockTextSet {
        id: BlockId(20),
        text: "nothing to find here".into(),
    }])
    .unwrap();
    assert!(hits(&repo, "quarter").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
