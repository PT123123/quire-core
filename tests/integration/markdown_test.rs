// M8 Markdown import/export acceptance tests (SPEC §二十六):
// per-kind export formatting, import recognition and edge cases, inline marks
// in both directions (M6), and the export -> import -> export semantic round
// trip (Chinese and CRLF).

use quire::core::persistence::Change;
use quire::core::types::{
    AttachmentId, Block, BlockId, BlockKind, Mark, MarkKind, OrderKey, Page, PageId,
};
use quire::services::export_service::export_page;
use quire::services::import_service::{import_markdown, parse_inline, parse_markdown, ParsedBlock};

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
        color: quire::core::ColorKind::Default,
        background: quire::core::ColorKind::Default,
        page_ref: None,
        folded: false,
        attachment: None,
        img_percent: 100,
        columns: 0,
    }
}

fn todo(id: u64, text: &str, checked: bool) -> Block {
    Block {
        checked,
        ..block(id, BlockKind::Todo, text)
    }
}

/// A mark span, spelled the way the model stores it.
fn mark(start: usize, end: usize, kind: MarkKind) -> Mark {
    Mark {
        start,
        end,
        kind,
        url: String::new(),
    }
}

fn link(start: usize, end: usize, url: &str) -> Mark {
    Mark {
        start,
        end,
        kind: MarkKind::Link,
        url: url.into(),
    }
}

fn with_marks(b: Block, marks: Vec<Mark>) -> Block {
    Block { marks, ..b }
}

/// A parsed block with no marks, which most of the block-level cases have.
fn parsed(kind: BlockKind, text: &str) -> ParsedBlock {
    ParsedBlock {
        kind,
        text: text.into(),
        checked: false,
        marks: Vec::new(),
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
            marks: b.marks.clone(),
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
        block(
            12,
            BlockKind::Quote,
            "Simplicity is the ultimate sophistication.",
        ),
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
fn rich_paste_gate_admits_structure_and_rejects_plain_text() {
    use quire::services::import_service::parse_if_block_structure;

    // multi-block text lands as blocks
    let parsed = parse_if_block_structure("# Title\n\nbody").unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].kind, BlockKind::Heading1);

    // a lone non-paragraph line is structure too
    let parsed = parse_if_block_structure("- one item").unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].kind, BlockKind::Bullet);

    // a todo line is structure even unchecked (the parser wants the
    // list-marker form)
    let parsed = parse_if_block_structure("- [ ] buy milk").unwrap();
    assert_eq!(parsed[0].kind, BlockKind::Todo);
    assert!(!parsed[0].checked);

    // plain single paragraphs stay native paste, marks included
    assert!(parse_if_block_structure("just a sentence").is_none());
    assert!(parse_if_block_structure("**bold** and *italic*").is_none());
    assert!(parse_if_block_structure("").is_none());
}

#[test]
fn export_page_block_writes_an_in_app_link() {
    // a Page block exports as a quire://page link, so re-import keeps the
    // target openable (the app resolves quire://page links in place). A
    // Link-to-page block exports the same way — the ownership difference
    // does not survive Markdown.
    let mut child = block(2, BlockKind::Page, "Project Atlas");
    child.page_ref = Some(PageId(9));
    let mut link = block(3, BlockKind::Link, "Research Notes");
    link.page_ref = Some(PageId(3));
    let md = export_page(&[
        block(1, BlockKind::Paragraph, "before"),
        child,
        link,
    ]);
    assert_eq!(
        md,
        "before\n\n[Project Atlas](quire://page/9)\n\n[Research Notes](quire://page/3)\n"
    );
    // a dangling reference degrades to plain text
    let md = export_page(&[block(4, BlockKind::Page, "Ghost")]);
    assert_eq!(md, "Ghost\n");
}

#[test]
fn export_writes_a_picture_as_a_link_that_keeps_its_name() {
    let pic = Block {
        attachment: Some(AttachmentId(12)),
        ..block(5, BlockKind::Image, "sunset.png")
    };
    let md = export_page(&[pic.clone()]);
    assert_eq!(md, "![sunset.png](quire://attachment/12)\n");

    // a picture whose file row is gone still names itself
    let dangling = Block { attachment: None, ..pic };
    assert_eq!(export_page(&[dangling]), "![sunset.png]()\n");

    // the importer has no picture shape, so the file name must survive as
    // text rather than vanish along with the syntax
    let back = parse_markdown(&md);
    assert_eq!(back.len(), 1);
    assert!(back[0].text.contains("sunset.png"), "got {:?}", back[0].text);
}

#[test]
fn export_writes_a_file_as_a_link_that_reopens_it() {
    let file = Block {
        attachment: Some(AttachmentId(12)),
        ..block(5, BlockKind::File, "quarterly-report.pdf")
    };
    // Unlike a picture, a file degrades to a *link* rather than an image: the
    // importer parses `[text](url)` into a marked run, so the round trip keeps
    // both the name and a way back to the bytes.
    let md = export_page(&[file.clone()]);
    assert_eq!(md, "[quarterly-report.pdf](quire://attachment/12)\n");

    let dangling = Block { attachment: None, ..file };
    assert_eq!(export_page(&[dangling]), "quarterly-report.pdf\n");

    let back = parse_markdown(&md);
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].text, "quarterly-report.pdf");
    assert!(
        back[0].marks.iter().any(|m| m.kind == MarkKind::Link
            && m.url == "quire://attachment/12"),
        "the link has to survive as a mark, got {:?}",
        back[0]
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

/// CommonMark has no collapsible section, so a Toggle leaves as a quote —
/// the same degradation Callout already accepts. Fold state is editor view
/// state: it neither reaches the file nor hides anything from it.
#[test]
fn export_degrades_a_toggle_but_keeps_its_whole_section() {
    let parent = Block {
        folded: true,
        ..block(1, BlockKind::Toggle, "Notes")
    };
    let child = Block {
        parent: Some(BlockId(1)),
        ..block(2, BlockKind::Bullet, "inside")
    };
    let md = export_page(&[parent, child]);
    assert_eq!(md, "> Notes\n\n  - inside\n");
    // and the text comes back on re-import, just no longer collapsible
    let back = import_markdown(&md, &page(), &mut counter(50));
    let texts: Vec<String> = blocks_of(&back).into_iter().map(|b| b.text).collect();
    assert_eq!(texts, vec!["Notes", "inside"]);
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
            (
                BlockKind::Numbered,
                "seventh (source number is ignored)",
                false
            ),
            (BlockKind::Numbered, "paren style", false),
            (BlockKind::Quote, "quoted line", false),
            (BlockKind::Divider, "", false),
            (BlockKind::Code, "let x = 1;", false),
        ]
    );
}

#[test]
fn import_keeps_unrecognized_markdown_as_literal_text() {
    let got = parse_markdown("#1 issue\n-ish\n2 * 3 * 4\nsnake_case_name\n~~~\ncode\n~~~\n");
    assert_eq!(
        got,
        vec![
            parsed(BlockKind::Paragraph, "#1 issue"),
            parsed(BlockKind::Paragraph, "-ish"),
            parsed(BlockKind::Paragraph, "2 * 3 * 4"),
            parsed(BlockKind::Paragraph, "snake_case_name"),
            parsed(BlockKind::Code, "code"),
        ]
    );
}

#[test]
fn import_collapses_deep_headings_and_flattens_nested_lists() {
    let got = parse_markdown("###### six\n- top\n  - nested\n    - deeper\n");
    assert_eq!(
        got,
        vec![
            parsed(BlockKind::Heading3, "six"),
            parsed(BlockKind::Bullet, "top"),
            parsed(BlockKind::Bullet, "nested"),
            parsed(BlockKind::Bullet, "deeper"),
        ]
    );
}

#[test]
fn import_handles_crlf_and_an_unterminated_fence() {
    let src = "# Title\r\n\r\n- item\r\n\r\n```\r\nlet x = 1;\r\n";
    assert_eq!(
        parse_markdown(src),
        vec![
            parsed(BlockKind::Heading1, "Title"),
            parsed(BlockKind::Bullet, "item"),
            parsed(BlockKind::Code, "let x = 1;"),
        ]
    );
}

#[test]
fn import_chinese_survives_unchanged() {
    let src =
        "## 写作与中文测试\n\n中文段落用于验证字体回退与行高。\n- 检查行高\n- [x] 完成引号方向\n";
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

// ── inline marks (M6) ───────────────────────────────────────────────

#[test]
fn import_turns_markers_into_mark_spans() {
    let doc =
        parse_markdown("**bold** and *it* and `code` and ~~gone~~ and [docs](https://quire.local)");
    let got = &doc[0];
    assert_eq!(got.text, "bold and it and code and gone and docs");
    assert_eq!(
        got.marks,
        vec![
            mark(0, 4, MarkKind::Bold),
            mark(9, 11, MarkKind::Italic),
            mark(16, 20, MarkKind::Code),
            mark(25, 29, MarkKind::Strike),
            link(34, 38, "https://quire.local"),
        ]
    );
}

#[test]
fn import_nests_a_span_inside_its_outer_one() {
    // the model has no tree, so nesting is the ranges themselves: the inner
    // span sits inside the outer, which is what the renderer walks
    let doc = parse_markdown("**a _b_ c**");
    assert_eq!(doc[0].text, "a b c");
    assert_eq!(
        doc[0].marks,
        vec![mark(0, 5, MarkKind::Bold), mark(2, 3, MarkKind::Italic)]
    );

    let doc = parse_markdown("[**label**](https://x.dev)");
    assert_eq!(doc[0].text, "label");
    assert_eq!(
        doc[0].marks,
        vec![mark(0, 5, MarkKind::Bold), link(0, 5, "https://x.dev")]
    );
}

#[test]
fn import_reads_three_markers_as_both_styles() {
    let doc = parse_markdown("***both*** stays one span");
    assert_eq!(doc[0].text, "both stays one span");
    assert_eq!(
        doc[0].marks,
        vec![mark(0, 4, MarkKind::Bold), mark(0, 4, MarkKind::Italic)]
    );
}

#[test]
fn import_leaves_ambiguous_markers_as_text() {
    for src in [
        "2 * 3 * 4",       // spaced operators are not emphasis
        "snake_case_name", // `_` may not open or close inside a word
        "~single~",        // strike needs two on each side
        "*unclosed",
        "\\*literal\\*", // an escape is text, not a marker
    ] {
        let (text, marks) = parse_inline(src);
        assert_eq!(text, src.replace("\\*", "*"), "{src} lost text");
        assert!(marks.is_empty(), "{src} marked as {marks:?}");
    }
}

#[test]
fn import_mark_offsets_are_byte_offsets_in_chinese_text() {
    let doc = parse_markdown("**加粗**与`代码`和[链接](https://例子.测试)");
    let got = &doc[0];
    assert_eq!(got.text, "加粗与代码和链接");
    assert_eq!(
        got.marks,
        vec![
            mark(0, 6, MarkKind::Bold),
            mark(9, 15, MarkKind::Code),
            link(18, 24, "https://例子.测试"),
        ]
    );
    // every offset lands on a char boundary, as the caret model requires
    for m in &got.marks {
        assert!(
            got.text.is_char_boundary(m.start) && got.text.is_char_boundary(m.end),
            "{m:?} cuts a character"
        );
    }
}

#[test]
fn import_attaches_marks_to_the_change_it_emits() {
    let mut alloc = counter(1);
    let blocks = blocks_of(&import_markdown(
        "- [x] ~~done~~ *today*",
        &page(),
        &mut alloc,
    ));
    assert_eq!(blocks[0].text, "done today");
    assert!(blocks[0].checked);
    assert_eq!(
        blocks[0].marks,
        vec![mark(0, 4, MarkKind::Strike), mark(5, 10, MarkKind::Italic)]
    );
}

#[test]
fn export_writes_marks_back_as_markers() {
    let blocks = vec![
        with_marks(
            block(1, BlockKind::Heading2, "Styled"),
            vec![mark(0, 6, MarkKind::Bold)],
        ),
        with_marks(
            block(2, BlockKind::Paragraph, "a bold bit"),
            vec![mark(2, 6, MarkKind::Bold)],
        ),
        with_marks(
            block(3, BlockKind::Bullet, "see the docs"),
            vec![link(4, 12, "https://quire.local")],
        ),
        with_marks(
            block(4, BlockKind::Todo, "run cargo test now"),
            vec![mark(4, 14, MarkKind::Code)],
        ),
    ];
    assert_eq!(
        export_page(&blocks),
        "## **Styled**\n\
         \na **bold** bit\n\
         \n- see [the docs](https://quire.local)\n\
         \n- [ ] run `cargo test` now\n"
    );
}

#[test]
fn export_escapes_markers_that_are_only_text() {
    // a paragraph that *says* `**bold**` must not come back styled
    let blocks = vec![block(1, BlockKind::Paragraph, "**bold** and * and ~ and `")];
    let md = export_page(&blocks);
    assert_eq!(md, "\\*\\*bold\\*\\* and \\* and \\~ and \\`\n");
    assert_eq!(parse_markdown(&md), parsed_of(&blocks));
}

#[test]
fn export_pads_a_code_span_that_would_merge_with_its_fence() {
    let blocks = vec![
        // a backtick at either end needs the space wrapper, and a content that
        // is itself padded survives the reader's strip
        with_marks(
            block(1, BlockKind::Paragraph, "a ` b"),
            vec![mark(2, 4, MarkKind::Code)],
        ),
        with_marks(
            block(2, BlockKind::Paragraph, " spaced "),
            vec![mark(0, 8, MarkKind::Code)],
        ),
        with_marks(
            block(3, BlockKind::Paragraph, "  "),
            vec![mark(0, 2, MarkKind::Code)],
        ),
        with_marks(
            block(4, BlockKind::Paragraph, "a``b"),
            vec![mark(1, 3, MarkKind::Code)],
        ),
    ];
    let md = export_page(&blocks);
    let mut alloc = counter(100);
    let again = blocks_of(&import_markdown(&md, &page(), &mut alloc));
    assert_eq!(parsed_of(&again), parsed_of(&blocks), "{md}");
}

#[test]
fn export_splits_marks_that_only_partially_overlap() {
    // CommonMark's inline tree cannot hold `bold 0..6` with `italic 3..9`: the
    // writer cuts both spans at the crossing so every character keeps its text
    // and each piece keeps a style
    let blocks = vec![with_marks(
        block(1, BlockKind::Paragraph, "abcdefghi"),
        vec![mark(0, 6, MarkKind::Bold), mark(3, 9, MarkKind::Italic)],
    )];
    let md = export_page(&blocks);
    assert_eq!(md, "**abc*****def****ghi*\n");
    let doc = parse_markdown(&md);
    assert_eq!(doc[0].text, "abcdefghi");
    assert_eq!(
        doc[0].marks,
        vec![
            mark(0, 3, MarkKind::Bold),
            mark(3, 6, MarkKind::Bold),
            mark(3, 6, MarkKind::Italic),
            mark(6, 9, MarkKind::Italic),
        ]
    );
    // and the file is a fixpoint: the pieces merge and split identically twice
    let mut alloc = counter(100);
    let again = blocks_of(&import_markdown(&md, &page(), &mut alloc));
    assert_eq!(export_page(&again), md);
}

#[test]
fn export_leaves_a_code_span_unstyled() {
    // a span inside a code span has no spelling at all: the code keeps the
    // text verbatim and the inner style goes
    let blocks = vec![with_marks(
        block(1, BlockKind::Paragraph, "abc def"),
        vec![mark(0, 3, MarkKind::Code), mark(1, 2, MarkKind::Italic)],
    )];
    let md = export_page(&blocks);
    assert_eq!(md, "`abc` def\n");
    assert_eq!(
        parse_markdown(&md)[0].marks,
        vec![mark(0, 3, MarkKind::Code)]
    );
}

#[test]
fn export_writes_a_link_target_that_needs_angle_brackets() {
    let blocks = vec![with_marks(
        block(1, BlockKind::Paragraph, "the spec"),
        vec![link(4, 8, "https://example.com/a b(1)")],
    )];
    let md = export_page(&blocks);
    assert_eq!(md, "the [spec](<https://example.com/a b(1)>)\n");
    assert_eq!(parse_markdown(&md)[0].marks, blocks[0].marks);
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
fn export_of_import_is_stable_for_inline_marks() {
    assert_round_trip(
        "styled document",
        "# **Styled** headings\n\nAn *emphasis*, a `span`, ~~struck~~ and a \n\
         [link](https://quire.local/docs) in one paragraph.\n\n\
         - [x] ship `cargo test` **today**\n\n\
         > quoted **words** with a `code` bit\n\n\
         中文段落：这是**加粗**、`代码`和[链接](https://例子.测试)。\n\n\
         \\*\\*literal\\*\\* stays literal.\n",
    );
}

#[test]
fn import_of_export_keeps_marks_for_the_editor_shape() {
    // the shapes the editor builds by toggling marks, written out and read back
    let blocks = vec![
        with_marks(
            block(1, BlockKind::Heading1, "Release notes"),
            vec![mark(0, 7, MarkKind::Bold)],
        ),
        with_marks(
            block(2, BlockKind::Paragraph, "both"),
            vec![mark(0, 4, MarkKind::Bold), mark(0, 4, MarkKind::Italic)],
        ),
        with_marks(
            block(3, BlockKind::Bullet, "see the changelog"),
            vec![link(4, 13, "https://quire.local/changelog")],
        ),
        with_marks(
            block(4, BlockKind::Paragraph, "加粗里的斜体"),
            vec![mark(0, 18, MarkKind::Bold), mark(3, 9, MarkKind::Italic)],
        ),
        block(5, BlockKind::Code, "let **x** = 1; // not styled"),
    ];
    let md = export_page(&blocks);
    let mut alloc = counter(100);
    let again = blocks_of(&import_markdown(&md, &page(), &mut alloc));
    assert_eq!(parsed_of(&again), parsed_of(&blocks), "{md}");
    assert_eq!(export_page(&again), md);
}

#[test]
fn import_of_export_is_stable_for_the_app_sample_shape() {
    // what the editor holds today: every kind, one page, Chinese included
    let blocks = vec![
        block(1, BlockKind::Paragraph, "A quiet home for thinking."),
        block(2, BlockKind::Divider, ""),
        block(3, BlockKind::Heading2, "Why a local-first editor"),
        block(
            4,
            BlockKind::Quote,
            "Simplicity is the ultimate sophistication.",
        ),
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

// ---- table grid (SPEC §三十七 批次 B) ----

/// A table plus its cells row-major, ids and orders following the table.
fn grid(table_id: u64, cols: u16, texts: &[&str]) -> Vec<Block> {
    let mut out = vec![Block {
        columns: cols,
        ..block(table_id, BlockKind::Table, "")
    }];
    for (i, t) in texts.iter().enumerate() {
        out.push(Block {
            parent: Some(BlockId(table_id)),
            ..block(table_id * 100 + i as u64, BlockKind::TableCell, t)
        });
    }
    out
}

#[test]
fn export_writes_a_grid_as_a_gfm_table_with_a_header_row() {
    let mut blocks = grid(1, 3, &["North", "South", "East", "West", "Up", "Down"]);
    blocks.push(block(2, BlockKind::Paragraph, "after"));
    assert_eq!(
        export_page(&blocks),
        "| North | South | East |\n| --- | --- | --- |\n| West | Up | Down |\n\nafter\n"
    );
}

#[test]
fn export_keeps_marks_in_a_cell_and_escapes_what_would_end_the_column() {
    // §十 inline marks ride into the grid: a cell exports like a line of prose
    let mut blocks = grid(1, 2, &[]);
    let bold = |id: u64, text: &str, marks: Vec<Mark>| Block {
        parent: Some(BlockId(1)),
        marks,
        ..block(id, BlockKind::TableCell, text)
    };
    blocks.push(bold(100, "Comms", vec![mark(0, 5, MarkKind::Bold)]));
    blocks.push(bold(101, "a|b", vec![]));
    blocks.push(bold(102, "two\nlines", vec![]));
    blocks.push(bold(103, "", vec![]));
    blocks.push(bold(104, "`x`", vec![]));
    blocks.push(bold(105, "中文 · ok", vec![]));
    assert_eq!(
        export_page(&blocks),
        // the cell text is prose, so the inline escaper runs on it first: a
        // backtick that is only a backtick stays only a backtick (ADR-0016)
        "| **Comms** | a\\|b |\n| --- | --- |\n| two lines |  |\n| \\`x\\` | 中文 · ok |\n"
    );
}

#[test]
fn a_grid_with_nothing_in_it_exports_to_nothing() {
    assert_eq!(export_page(&grid(1, 3, &[])), "");
    // a cell without its table writes no text of its own: it is only ever a
    // piece of a grid, and a piece with no grid has no place in the document
    let orphan = Block { parent: Some(BlockId(9)), ..block(5, BlockKind::TableCell, "loose") };
    assert_eq!(export_page(&[orphan]), "");
}

#[test]
fn a_gfm_table_imports_as_text_not_as_a_grid() {
    // documented limitation: the importer recognizes no table syntax, so a
    // Markdown table file comes in as lines rather than breaking on the pipes
    let src = "| a | b |\n| --- | --- |\n| c | d |\n";
    let mut alloc = counter(100);
    let blocks = blocks_of(&import_markdown(src, &page(), &mut alloc));
    assert!(blocks.iter().all(|b| b.kind != BlockKind::Table), "{:?}", blocks.iter().map(|b| (b.kind, &b.text)).collect::<Vec<_>>());
    assert!(blocks.iter().all(|b| b.kind != BlockKind::TableCell));
    assert!(blocks.iter().any(|b| b.text.contains('a')), "the pipes keep their text");
}

// ---- columns layout (SPEC §三十七 批次 B) ----

/// A two-box layout: `before`, [left | right], `after`, with the lines
/// parented to their box and the boxes to the layout.
fn laid_out() -> Vec<Block> {
    let col = |id: u64, parent: u64| Block {
        parent: Some(BlockId(parent)),
        ..block(id, BlockKind::Column, "")
    };
    vec![
        block(1, BlockKind::Paragraph, "before"),
        Block { columns: 2, ..block(2, BlockKind::Columns, "") },
        col(3, 2),
        Block { parent: Some(BlockId(3)), ..block(4, BlockKind::Paragraph, "left") },
        col(5, 2),
        Block { parent: Some(BlockId(5)), ..todo(6, "right", true) },
        block(7, BlockKind::Paragraph, "after"),
    ]
}

#[test]
fn a_columns_layout_exports_its_lines_in_reading_order() {
    // the shape is a layout, not text, so Markdown has nothing to write for
    // it: the containers drop out and the lines come out as page-level prose
    assert_eq!(
        export_page(&laid_out()),
        "before\n\nleft\n\n- [x] right\n\nafter\n"
    );
}

#[test]
fn a_layout_flattens_its_boxes_nesting_too() {
    // a line under a line, both inside a box: the box's own depth is already
    // gone, so the inner indent would be a lie about the document
    let mut blocks = laid_out();
    blocks.push(Block {
        parent: Some(BlockId(4)),
        ..block(8, BlockKind::Bullet, "nested")
    });
    assert_eq!(
        export_page(&blocks),
        // no two-space indent in front of "- nested": the depth was flattened
        // with the box, and the paragraph/list break is the usual one
        "before\n\nleft\n\n- nested\n\n- [x] right\n\nafter\n"
    );
}

#[test]
fn an_empty_layout_exports_to_nothing() {
    // the same degradation a grid takes: a container with no content is not a
    // block of text, and a bare box is only a piece of a layout
    let only = vec![Block { columns: 3, ..block(1, BlockKind::Columns, "") }];
    assert_eq!(export_page(&only), "");
    let orphan = Block { parent: Some(BlockId(9)), ..block(5, BlockKind::Column, "") };
    assert_eq!(export_page(&[orphan]), "");
}

#[test]
fn a_layout_round_trips_as_its_lines_losing_only_the_shape() {
    let md = export_page(&laid_out());
    let mut alloc = counter(100);
    let again = blocks_of(&import_markdown(&md, &page(), &mut alloc));
    let expected = vec![
        parsed(BlockKind::Paragraph, "before"),
        parsed(BlockKind::Paragraph, "left"),
        ParsedBlock {
            kind: BlockKind::Todo,
            text: "right".into(),
            checked: true,
            marks: Vec::new(),
        },
        parsed(BlockKind::Paragraph, "after"),
    ];
    assert_eq!(parsed_of(&again), expected);
    // and exporting that again is byte-identical
    assert_eq!(export_page(&again), md);
}

// ── math (SPEC §三十七 批次 C, ADR-0038) ────────────────────────────

#[test]
fn a_math_block_exports_inside_its_own_fence() {
    let blocks = vec![block(1, BlockKind::Math, r"\frac{a+b}{2}")];
    let md = export_page(&blocks);
    assert_eq!(md, "$$\n\\frac{a+b}{2}\n$$\n");
    assert_eq!(
        parse_markdown(&md),
        vec![parsed(BlockKind::Math, r"\frac{a+b}{2}")]
    );
}

#[test]
fn a_math_fence_is_verbatim_where_prose_would_unescape() {
    // the control that says the fence really is a fence: the same bytes as a
    // paragraph lose the backslash in front of a comma, as a formula they do
    // not, because `\,` is a spacing command and not an escaped comma
    let fenced = parse_markdown(r#"$$
\alpha \, \beta
$$"#);
    assert_eq!(fenced[0].text, r"\alpha \, \beta");
    let prose = parse_markdown(r"\alpha \, \beta");
    assert_eq!(prose[0].text, r"\alpha , \beta");
}

#[test]
fn an_inline_formula_marks_its_span_and_reads_back_the_same_bytes() {
    let (text, marks) = parse_inline("mass $E = mc^2$ here");
    assert_eq!(text, "mass E = mc^2 here");
    assert_eq!(marks, vec![mark(5, 13, MarkKind::Math)]);
    let md = export_page(&[with_marks(
        block(1, BlockKind::Paragraph, &text),
        marks,
    )]);
    assert_eq!(md, "mass $E = mc^2$ here\n");
}

#[test]
fn prose_holding_two_dollars_is_not_a_formula() {
    // what makes `$…$` safe to read at all: a dollar needs a non-space on the
    // inside of both its delimiters, which money amounts never present
    let (text, marks) = parse_inline("costs $5 and $10 today");
    assert_eq!(text, "costs $5 and $10 today");
    assert!(marks.is_empty(), "{marks:?}");
}

#[test]
fn export_escapes_the_dollar_that_would_otherwise_re_form_as_math() {
    let md = export_page(&[block(1, BlockKind::Paragraph, "a$b$c")]);
    assert_eq!(md, "a\\$b$c\n");
    let back = &parse_markdown(&md)[0];
    assert_eq!(back.text, "a$b$c");
    assert!(back.marks.is_empty(), "{:?}", back.marks);
    // and prose that only *has* dollars keeps them bare
    assert_eq!(
        export_page(&[block(1, BlockKind::Paragraph, "costs $5 and $10")]),
        "costs $5 and $10\n"
    );
}

#[test]
fn a_formula_span_holds_no_other_style() {
    // the `$…$` content is source, so a bold sharing it has no spelling: the
    // mark goes and the text stays exact, like inside a code span
    let b = with_marks(
        block(1, BlockKind::Paragraph, "ab"),
        vec![mark(0, 2, MarkKind::Bold), mark(0, 2, MarkKind::Math)],
    );
    let md = export_page(&[b]);
    assert_eq!(md, "$ab$\n");
    assert_eq!(
        parse_markdown(&md)[0].marks,
        vec![mark(0, 2, MarkKind::Math)]
    );
}

#[test]
fn a_space_padded_formula_exports_as_text_not_as_a_broken_fence() {
    // `$ x $` never imports back as a formula, so writing it would move two
    // dollars into the user's text; the mark goes instead
    let b = with_marks(
        block(1, BlockKind::Paragraph, "start x end"),
        vec![mark(5, 8, MarkKind::Math)],
    );
    let md = export_page(&[b]);
    assert_eq!(md, "start x end\n");
    assert_eq!(parse_markdown(&md)[0].text, "start x end");
    assert!(parse_markdown(&md)[0].marks.is_empty());
}

#[test]
fn a_math_block_imports_from_both_fence_shapes() {
    let fenced = parse_markdown("$$\nx^2\n$$\n");
    let inline = parse_markdown("$$x^2$$\n");
    assert_eq!(fenced, inline);
    assert_eq!(fenced[0].text, "x^2");
    // an unclosed fence keeps what it collected, like a code fence does
    let loose = parse_markdown("$$\nx^2\n");
    assert_eq!(loose, vec![parsed(BlockKind::Math, "x^2")]);
}

#[test]
fn math_round_trips_in_both_shapes() {
    // a raw literal with real line breaks, because the bytes that matter here
    // are backslashes and `\n` would have to survive two readers to get wrong
    assert_round_trip(
        "math",
        r#"$$
\frac{a+b}{2} \leq \sqrt{ab}
$$

The mass is $E = mc^2$ here.
"#,
    );
}

// ── contents block (SPEC §三十七 批次 C) ────────────────────────────

#[test]
fn a_contents_block_exports_one_marker_line_and_reads_back() {
    // the list is derived from the page's headings, so the file carries only
    // the fact that a contents block sits here — writing its lines would put
    // block ids from this library into a document that has none
    let md = export_page(&[block(1, BlockKind::Toc, "")]);
    assert_eq!(md, "<!-- quire:toc -->\n");
    assert_eq!(parse_markdown(&md), vec![parsed(BlockKind::Toc, "")]);
}

#[test]
fn a_marker_line_neither_vanishes_nor_swallows_its_neighbours() {
    // the control for "an HTML comment is dropped by a Markdown reader": the
    // line survives as one block and the prose around it stays where it was
    let blocks = vec![
        block(1, BlockKind::Heading1, "Top"),
        block(2, BlockKind::Toc, ""),
        block(3, BlockKind::Paragraph, "below"),
    ];
    let md = export_page(&blocks);
    assert_eq!(md, "# Top\n\n<!-- quire:toc -->\n\nbelow\n");
    assert_eq!(
        parse_markdown(&md),
        vec![
            parsed(BlockKind::Heading1, "Top"),
            parsed(BlockKind::Toc, ""),
            parsed(BlockKind::Paragraph, "below"),
        ]
    );
}

#[test]
fn a_contents_block_round_trips_without_a_copy_of_its_list() {
    assert_round_trip("contents marker", "<!-- quire:toc -->\n\n# H\n");
    let blocks = blocks_of(&import_markdown(
        "<!-- quire:toc -->\n",
        &page(),
        &mut counter(1),
    ));
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Toc);
    assert_eq!(blocks[0].text, "");
}
