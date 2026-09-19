// M7 in-page find acceptance tests (SPEC §二十): the hit list a Ctrl+F bar
// walks. Byte ranges per hit, forward and backward stepping with wrapping,
// Chinese offsets on char boundaries, and the empty-term shapes the bar hits
// while the user types.

use quire::core::types::{Block, BlockId, BlockKind, OrderKey, PageId};
use quire::services::find_service::{FindHit, FindSession};

fn block(id: u64, text: &str) -> Block {
    Block {
        id: BlockId(id),
        page: PageId(3),
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

fn hit(block: u64, start: usize, end: usize) -> FindHit {
    FindHit {
        block: BlockId(block),
        start,
        end,
    }
}

#[test]
fn a_session_walks_every_occurrence_in_display_order() {
    let blocks = vec![
        block(1, "the cat and the dog"),
        block(2, "THE bird"),
        block(3, "no match here"),
        block(4, "and the fish"),
    ];
    let mut s = FindSession::new("the", &blocks);
    assert_eq!(s.total(), 4);
    assert_eq!(s.term(), "the");
    assert_eq!(
        s.current(),
        None,
        "nothing is current before the first step"
    );
    assert_eq!(s.position(), None);

    assert_eq!(s.next(), Some(hit(1, 0, 3)));
    assert_eq!(s.next(), Some(hit(1, 12, 15)));
    assert_eq!(s.current(), Some(hit(1, 12, 15)));
    assert_eq!(s.next(), Some(hit(2, 0, 3)), "ASCII matching ignores case");
    assert_eq!(s.next(), Some(hit(4, 4, 7)));
    assert_eq!(s.position(), Some(4));
    // and wraps, in both directions
    assert_eq!(s.next(), Some(hit(1, 0, 3)));
    assert_eq!(s.position(), Some(1));
    assert_eq!(s.prev(), Some(hit(4, 4, 7)));
    assert_eq!(s.prev(), Some(hit(2, 0, 3)));
    let mut s = FindSession::new("the", &blocks);
    assert_eq!(
        s.prev(),
        Some(hit(4, 4, 7)),
        "the first step back ends last"
    );
}

#[test]
fn a_hit_range_addresses_its_own_block_text() {
    let blocks = vec![
        block(1, "the cat and the dog"),
        block(2, "THE bird"),
        block(3, "no match here"),
        block(4, "and the fish"),
    ];
    let s = FindSession::new("the", &blocks);
    assert_eq!(s.hits().len(), 4);
    for h in s.hits() {
        let text = &blocks
            .iter()
            .find(|b| b.id == h.block)
            .expect("a hit names a block of the page")
            .text;
        assert!(text.is_char_boundary(h.start) && text.is_char_boundary(h.end));
        assert_eq!(
            text[h.start..h.end].to_ascii_lowercase(),
            "the",
            "the range does not hold the term: {h:?}"
        );
    }
}

#[test]
fn the_walk_follows_the_order_the_caller_passes_in() {
    // display order, not id order: the editor already drew the page once, and
    // the bar has to step through what the reader sees
    let blocks = vec![
        block(9, "last but first"),
        block(3, "middle of the page"),
        block(1, "first of all"),
    ];
    let mut s = FindSession::new("fi", &blocks);
    assert_eq!(s.total(), 2);
    assert_eq!(s.next().map(|h| h.block), Some(BlockId(9)));
    assert_eq!(s.next().map(|h| h.block), Some(BlockId(1)));
}

#[test]
fn every_block_kind_contributes_hits() {
    let blocks = vec![
        Block {
            kind: BlockKind::Heading2,
            ..block(1, "Fresh start")
        },
        Block {
            kind: BlockKind::Todo,
            ..block(2, "restart the finder")
        },
        Block {
            kind: BlockKind::Code,
            ..block(3, "fn restart() {}")
        },
        Block {
            kind: BlockKind::Divider,
            ..block(4, "")
        },
    ];
    let s = FindSession::new("restart", &blocks);
    assert_eq!(
        s.hits().iter().map(|h| h.block.0).collect::<Vec<_>>(),
        vec![2, 3],
        "the code block is text too, the divider has none"
    );
}

#[test]
fn chinese_hits_are_byte_offsets_on_char_boundaries() {
    let blocks = vec![block(1, "中文段落用于验证字体回退，字体很重要")];
    let mut s = FindSession::new("字体", &blocks);
    assert_eq!(s.total(), 2);
    let first = s.next().unwrap();
    // 中文段落用于验证 is eight characters, three bytes each
    assert_eq!((first.start, first.end), (24, 30));
    assert_eq!(&blocks[0].text[first.start..first.end], "字体");
    let second = s.next().unwrap();
    // then 回退，(three more characters) before the second one
    assert_eq!((second.start, second.end), (39, 45));
    for h in s.hits() {
        assert!(
            blocks[0].text.is_char_boundary(h.start) && blocks[0].text.is_char_boundary(h.end),
            "{h:?} cuts a character in half"
        );
    }
}

#[test]
fn matching_never_overlaps_and_walks_left_to_right() {
    let blocks = vec![block(1, "aaaa")];
    let s = FindSession::new("aa", &blocks);
    assert_eq!(s.hits(), &[hit(1, 0, 2), hit(1, 2, 4)]);

    let blocks = vec![block(1, "ababab")];
    assert_eq!(FindSession::new("ab", &blocks).total(), 3);
}

#[test]
fn a_multiword_term_matches_the_spaces_it_types() {
    let blocks = vec![block(1, "ship it today"), block(2, "ship  it")];
    let s = FindSession::new("ship it", &blocks);
    assert_eq!(s.hits(), &[hit(1, 0, 7)]);
}

#[test]
fn a_term_that_matches_nothing_leaves_nothing_to_walk() {
    let blocks = vec![block(1, "hello"), block(2, "world")];
    for term in ["zzz", "", "   "] {
        let mut s = FindSession::new(term, &blocks);
        assert!(s.is_empty() && s.total() == 0, "{term:?} matched");
        assert_eq!(s.next(), None);
        assert_eq!(s.prev(), None);
        assert_eq!(s.current(), None);
        assert_eq!(s.position(), None);
        assert!(s.hits().is_empty());
    }
}

#[test]
fn a_new_term_starts_a_new_walk() {
    let blocks = vec![block(1, "one two three"), block(2, "two again")];
    let mut walked = FindSession::new("two", &blocks);
    assert_eq!(walked.next().map(|h| h.start), Some(4));
    assert_eq!(walked.next().map(|h| h.start), Some(0));

    let mut fresh = FindSession::new("three", &blocks);
    assert_eq!(fresh.total(), 1);
    assert_eq!(fresh.next(), Some(hit(1, 8, 13)));
    assert_eq!(
        fresh.next(),
        Some(hit(1, 8, 13)),
        "one hit wraps onto itself"
    );
}
