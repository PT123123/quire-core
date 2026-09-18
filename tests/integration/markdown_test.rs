// M8 Markdown import/export acceptance tests (SPEC §二十六):
// per-kind export formatting, import recognition and edge cases, and the
// export -> import -> export semantic round trip (Chinese and CRLF).

use quire::core::persistence::Change;
use quire::core::types::{Block, BlockId, BlockKind, OrderKey, Page, PageId};
use quire::services::export_service::export_page;
use quire::services::import_service::{import_markdown, parse_markdown, ParsedBlock};

fn block(id: u64, kind: BlockKind, text: &str) -> Block {
    Block {
        id: BlockId(id),
        page: PageId(7),
        parent: None,
        order: OrderKey(id * 10),
        kind,
        text: text.into(),
        checked: false,
        marks: Vec::new(),
    }
}

fn todo(id: u64, text: &str, checked: bool) -> Block {
    Block {
        checked,
        ..block(id, BlockKind::Todo, text)
    }
}

fn page() -> Page {
    Page {
        id: PageId(7),
        title: "Imported".into(),
        parent: None,
        order: OrderKey::FIRST,
        favorite: false,
        expanded: false,
    }
}

/// The allocator is injected: services never mint ids themselves.
fn counter(start: u64) -> impl FnMut() -> BlockId {
    let mut next = start;
    move || {
        let id = BlockId(next);
        next += 1;
        id
    }
}

/// Blocks out of a change list, ready to export again.
fn blocks_of(changes: &[Change]) -> Vec<Block> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::BlockInserted(b) => Some(b.clone()),
            _ => None,
        })
        .collect()
}

fn parsed_of(blocks: &[Block]) -> Vec<ParsedBlock> {
    blocks
        .iter()
        .map(|b| ParsedBlock {
            kind: b.kind,
            text: b.text.clone(),
            checked: b.checked,
        })
        .collect()
}

// ── export ──────────────────────────────────────────────────────────

#[test]
fn export_covers_every_block_kind() {
    let md = export_page(&[
        block(1, BlockKind::Heading1, "Title"),
        block(2, BlockKind::Heading2, "Section"),
        block(3, BlockKind::Heading3, "Subsection"),
        block(4, BlockKind::Paragraph, "A plain paragraph."),
        block(5, BlockKind::Bullet, "first"),
        block(6, BlockKind::Bullet, "second"),
        block(7, BlockKind::Numbered, "one"),
        block(8, BlockKind::Numbered, "two"),
        block(9, BlockKind::Numbered, "three"),
        todo(10, "open task", false),
        todo(11, "done task", true),
        block(12, BlockKind::Quote, "Simplicity is the ultimate sophistication."),
        block(13, BlockKind::Code, "cargo run --release"),
        block(14, BlockKind::Divider, ""),
        block(15, BlockKind::Paragraph, "中文段落，用于导出验证。"),
    ]);
    assert_eq!(
        md,
        "# Title\n\
         \n## Section\n\
         \n### Subsection\n\
         \nA plain paragraph.\n\
         \n- first\n- second\n\
         \n1. one\n2. two\n3. three\n\
         \n- [ ] open task\n- [x] done task\n\
         \n> Simplicity is the ultimate sophistication.\n\
         \n```\ncargo run --release\n```\n\
         \n---\n\
         \n中文段落，用于导出验证。\n"
    );
}

#[test]
fn export_numbered_runs_restart_after_any_other_block() {
    let md = export_page(&[
        block(1, BlockKind::Numbered, "a"),
        block(2, BlockKind::Numbered, "b"),
        block(3, BlockKind::Paragraph, "break"),
        block(4, BlockKind::Numbered, "c"),
    ]);
    assert_eq!(md, "1. a\n2. b\n\nbreak\n\n1. c\n");
}

#[test]
fn export_multiline_code_block_stays_inside_the_fence() {
    let md = export_page(&[block(
        1,
        BlockKind::Code,
        "fn main() {\n    println!(\"hi\");\n}",
    )]);
    assert_eq!(md, "```\nfn main() {\n    println!(\"hi\");\n}\n```\n");
}

#[test]
fn export_indents_child_blocks_and_keeps_them_under_the_parent() {
    let parent = block(1, BlockKind::Bullet, "parent");
    let child = Block {
        parent: Some(BlockId(1)),
        ..block(2, BlockKind::Bullet, "child")
    };
    let grand = Block {
        parent: Some(BlockId(2)),
        ..block(3, BlockKind::Bullet, "grandchild")
    };
    let md = export_page(&[grand, child, parent]);
    assert_eq!(md, "- parent\n  - child\n    - grandchild\n");
}

#[test]
fn export_empty_page_is_an_empty_file() {
    assert_eq!(export_page(&[]), "");
}

#[test]
fn export_always_ends_with_exactly_one_newline() {
    let cases: Vec<Vec<Block>> = vec![
        vec![block(1, BlockKind::Paragraph, "text")],
        vec![block(1, BlockKind::Divider, "")],
        vec![block(1, BlockKind::Paragraph, "multi\nline")],
        vec![
            block(1, BlockKind::Bullet, "a"),
            block(2, BlockKind::Bullet, "b"),
        ],
        vec![block(1, BlockKind::Code, "line1\nline2")],
        vec![block(1, BlockKind::Paragraph, "")],
    ];
    for blocks in cases {
        let md = export_page(&blocks);
        assert!(
            !md.ends_with("\n\n") && (md.is_empty() || md.ends_with('\n')),
            "trailing newline wrong for {:?}: {md:?}",
            blocks.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
    }
}

#[test]
fn export_sorts_by_order_key_not_input_order() {
    let late = Block {
        order: OrderKey(900),
        ..block(1, BlockKind::Paragraph, "late")
    };
    let early = Block {
        order: OrderKey(10),
        ..block(2, BlockKind::Paragraph, "early")
    };
    assert_eq!(export_page(&[late, early]), "early\n\nlate\n");
}

// ── import ──────────────────────────────────────────────────────────

#[test]
fn import_recognizes_every_supported_construct() {
    let src = "# H1\n\
               ## H2\n\
               ### H3\n\
               \n\
               A paragraph.\n\
               - dash bullet\n\
               * star bullet\n\
               + plus bullet\n\
               - [ ] unchecked\n\
               - [x] checked lower\n\
               - [X] checked upper\n\
               1. first\n\
               7. seventh (source number is ignored)\n\
               2) paren style\n\
               > quoted line\n\
               \n\
               ---\n\
               \n\
               ```\n\
               let x = 1;\n\
               ```\n";
    let parsed = parse_markdown(src);
    let kinds: Vec<(BlockKind, &str, bool)> = parsed
        .iter()
        .map(|b| (b.kind, b.text.as_str(), b.checked))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (BlockKind::Heading1, "H1", false),
            (BlockKind::Heading2, "H2", false),
            (BlockKind::Heading3, "H3", false),
            (BlockKind::Paragraph, "A paragraph.", false),
            (BlockKind::Bullet, "dash bullet", false),
            (BlockKind::Bullet, "star bullet", false),
            (BlockKind::Bullet, "plus bullet", false),
            (BlockKind::Todo, "unchecked", false),
            (BlockKind::Todo, "checked lower", true),
            (BlockKind::Todo, "checked upper", true),
            (BlockKind::Numbered, "first", false),
            (BlockKind::Numbered, "seventh (source number is ignored)", false),
            (BlockKind::Numbered, "paren style", false),
            (BlockKind::Quote, "quoted line", false),
            (BlockKind::Divider, "", false),
            (BlockKind::Code, "let x = 1;", false),
        ]
    );
}

#[test]
fn import_keeps_unrecognized_markdown_as_literal_text() {
    let parsed = parse_markdown("**bold** text\n#1 issue\n-ish\n~~~\ncode\n~~~\n");
    assert_eq!(
        parsed,
        vec![
            ParsedBlock {
                kind: BlockKind::Paragraph,
                text: "**bold** text".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Paragraph,
                text: "#1 issue".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Paragraph,
                text: "-ish".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Code,
                text: "code".into(),
                checked: false
            },
        ]
    );
}

#[test]
fn import_collapses_deep_headings_and_flattens_nested_lists() {
    let parsed = parse_markdown("###### six\n- top\n  - nested\n    - deeper\n");
    assert_eq!(
        parsed,
        vec![
            ParsedBlock {
                kind: BlockKind::Heading3,
                text: "six".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Bullet,
                text: "top".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Bullet,
                text: "nested".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Bullet,
                text: "deeper".into(),
                checked: false
            },
        ]
    );
}

#[test]
fn import_handles_crlf_and_an_unterminated_fence() {
    let src = "# Title\r\n\r\n- item\r\n\r\n```\r\nlet x = 1;\r\n";
    assert_eq!(
        parse_markdown(src),
        vec![
            ParsedBlock {
                kind: BlockKind::Heading1,
                text: "Title".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Bullet,
                text: "item".into(),
                checked: false
            },
            ParsedBlock {
                kind: BlockKind::Code,
                text: "let x = 1;".into(),
                checked: false
            },
        ]
    );
}

#[test]
fn import_chinese_survives_unchanged() {
    let src = "## 写作与中文测试\n\n中文段落用于验证字体回退与行高。\n- 检查行高\n- [x] 完成引号方向\n";
    let parsed = parse_markdown(src);
    assert_eq!(parsed[0].text, "写作与中文测试");
    assert_eq!(parsed[1].text, "中文段落用于验证字体回退与行高。");
    assert_eq!(parsed[2].text, "检查行高");
    assert_eq!(parsed[3].text, "完成引号方向");
    assert!(parsed[3].checked);
}

#[test]
fn import_empty_document_creates_an_empty_page() {
    let changes = import_markdown("\n \n", &page(), &mut counter(500));
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], Change::PageCreated(_)));
}

#[test]
fn import_emits_page_created_then_one_block_per_line_with_injected_ids() {
    let mut alloc = counter(9_000);
    let changes = import_markdown("# T\n\nbody\n- list\n", &page(), &mut alloc);
    assert_eq!(changes.len(), 4);

    assert_eq!(
        changes[0],
        Change::PageCreated(Page {
            id: PageId(7),
            title: "Imported".into(),
            parent: None,
            order: OrderKey::FIRST,
            favorite: false,
            expanded: false,
        })
    );

    let blocks = blocks_of(&changes);
    assert_eq!(
        blocks.iter().map(|b| b.id).collect::<Vec<_>>(),
        vec![BlockId(9_000), BlockId(9_001), BlockId(9_002)],
        "ids must come from the caller's allocator"
    );
    for b in &blocks {
        assert_eq!(b.page, PageId(7));
        assert_eq!(b.parent, None, "imported pages are flat");
    }
    assert!(
        blocks.windows(2).all(|w| w[0].order < w[1].order),
        "order keys must be increasing: {:?}",
        blocks.iter().map(|b| b.order.0).collect::<Vec<_>>()
    );
}

// ── round trip ──────────────────────────────────────────────────────

fn assert_round_trip(label: &str, src: &str) {
    let mut alloc = counter(1);
    let blocks = blocks_of(&import_markdown(src, &page(), &mut alloc));
    let reimported = parse_markdown(&export_page(&blocks));
    assert_eq!(reimported, parsed_of(&blocks), "round trip broke {label}");
}

#[test]
fn export_of_import_is_semantically_stable() {
    assert_round_trip(
        "mixed document",
        "# Heading one\n\nIntro paragraph.\n\n- bullet\n* second bullet\n\n\
         1. one\n2. two\n3. three\n\n- [ ] todo\n- [x] finished\n\n\
         > a quote\n\n```\nfn main() {}\n```\n\n---\n\nplain ending.\n",
    );
}

#[test]
fn export_of_import_is_stable_for_chinese() {
    assert_round_trip(
        "chinese document",
        "# 标题\n\n中文段落，包含 English words 和数字 2026。\n\n- 列表项\n- [x] 已完成\n\n\
         > 引用：好的排版是看不见的。\n\n```\ncargo run --release\n```\n",
    );
}

#[test]
fn export_of_import_is_stable_for_crlf_input() {
    assert_round_trip("crlf document", "# A\r\n\r\nbody\r\n\r\n- b\r\n");
}

#[test]
fn import_of_export_is_stable_for_the_app_sample_shape() {
    // what the editor holds today: every kind, one page, Chinese included
    let blocks = vec![
        block(1, BlockKind::Paragraph, "A quiet home for thinking."),
        block(2, BlockKind::Divider, ""),
        block(3, BlockKind::Heading2, "Why a local-first editor"),
        block(4, BlockKind::Quote, "Simplicity is the ultimate sophistication."),
        block(5, BlockKind::Bullet, "One process, one document model"),
        block(6, BlockKind::Numbered, "Write instantly"),
        block(7, BlockKind::Numbered, "Scroll a 10 000-block page"),
        todo(8, "Block editor MVP", false),
        block(9, BlockKind::Heading2, "运行与中文"),
        block(
            10,
            BlockKind::Paragraph,
            "中文段落用于验证字体回退与行高：排版本应稳定。",
        ),
        block(11, BlockKind::Code, "cargo run --release  # 140 ms"),
    ];
    let md = export_page(&blocks);
    let mut alloc = counter(100);
    let again = blocks_of(&import_markdown(&md, &page(), &mut alloc));
    assert_eq!(parsed_of(&again), parsed_of(&blocks));
    // exporting twice is byte-identical
    assert_eq!(export_page(&again), md);
}
