// Core model types — the contract between the M4 editor (produces Changes)
// and the M3 storage layer (persists them). Data only: no Slint, no IO.
//
// Design notes:
// - Ids are newtyped u64s, stable forever, never array indices (SPEC §九).
// - Sibling order is a dense u64 key (`OrderKey`); insertion between two
//   siblings takes the midpoint, and callers renumber the run when the gap
//   runs out. Storage persists it as an integer, nothing else cares.
// - `text` is plain UTF-8 in M4; inline marks (M6) extend this shape
//   without changing the identity/persistence rules.

use std::collections::BTreeMap;
use std::fmt;

macro_rules! id_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl $name {
            pub fn as_u64(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

id_newtype!(PageId, "Stable identifier of a page.");
id_newtype!(BlockId, "Stable identifier of a block.");

/// Sibling position. Smaller sorts first; unique among siblings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey(pub u64);

impl OrderKey {
    pub const FIRST: OrderKey = OrderKey(1 << 32);

    /// Midpoint between two keys; `None` when the gap is exhausted and the
    /// caller must renumber the sibling run.
    pub fn between(before: Option<OrderKey>, after: Option<OrderKey>) -> Option<OrderKey> {
        match (before, after) {
            (None, None) => Some(Self::FIRST),
            (None, Some(a)) => a.0.checked_sub(2).map(|v| OrderKey(v / 2 + 1)),
            // append at the end: u64 gives unlimited headroom
            (Some(b), None) => b.0.checked_add(1).map(OrderKey),
            (Some(b), Some(a)) => {
                if a.0 >= b.0 + 2 {
                    Some(OrderKey(b.0 + (a.0 - b.0) / 2))
                } else {
                    None
                }
            }
        }
    }
}

/// The M4 block set (SPEC §九). Stored as short stable strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Paragraph,
    Heading1,
    Heading2,
    Heading3,
    Bullet,
    Numbered,
    Todo,
    Quote,
    Code,
    Divider,
    Callout,
}

impl BlockKind {
    pub const ALL: [BlockKind; 11] = [
        BlockKind::Paragraph,
        BlockKind::Heading1,
        BlockKind::Heading2,
        BlockKind::Heading3,
        BlockKind::Bullet,
        BlockKind::Numbered,
        BlockKind::Todo,
        BlockKind::Quote,
        BlockKind::Code,
        BlockKind::Divider,
        BlockKind::Callout,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Paragraph => "paragraph",
            BlockKind::Heading1 => "heading_1",
            BlockKind::Heading2 => "heading_2",
            BlockKind::Heading3 => "heading_3",
            BlockKind::Bullet => "bullet",
            BlockKind::Numbered => "numbered",
            BlockKind::Todo => "todo",
            BlockKind::Quote => "quote",
            BlockKind::Code => "code",
            BlockKind::Divider => "divider",
            BlockKind::Callout => "callout",
        }
    }

    pub fn try_from_str(s: &str) -> Option<BlockKind> {
        BlockKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// Block-level color (Notion-style). Applies to the block's text and, for
/// `background`, to the row behind it. `Default` means "inherit the theme".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorKind {
    Default,
    Gray,
    Brown,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Pink,
    Red,
}

impl ColorKind {
    pub const ALL: [ColorKind; 10] = [
        ColorKind::Default,
        ColorKind::Gray,
        ColorKind::Brown,
        ColorKind::Orange,
        ColorKind::Yellow,
        ColorKind::Green,
        ColorKind::Blue,
        ColorKind::Purple,
        ColorKind::Pink,
        ColorKind::Red,
    ];

    /// Palette slot for the UI (0 = default, 1.. = gray..red). The Slint
    /// side maps the slot to theme-aware colors.
    pub fn slot(self) -> i32 {
        self as u32 as i32
    }

    pub fn from_slot(slot: i32) -> Option<ColorKind> {
        if (0..ColorKind::ALL.len() as i32).contains(&slot) {
            Some(ColorKind::ALL[slot as usize])
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ColorKind::Default => "",
            ColorKind::Gray => "gray",
            ColorKind::Brown => "brown",
            ColorKind::Orange => "orange",
            ColorKind::Yellow => "yellow",
            ColorKind::Green => "green",
            ColorKind::Blue => "blue",
            ColorKind::Purple => "purple",
            ColorKind::Pink => "pink",
            ColorKind::Red => "red",
        }
    }

    pub fn try_from_str(s: &str) -> Option<ColorKind> {
        ColorKind::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == s)
    }
}

/// Inline mark styling (M6). Offsets are byte offsets into the block's
/// UTF-8 text (char boundaries — the caret model guarantees it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Bold,
    Italic,
    Strike,
    Code,
    Link,
}

impl MarkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MarkKind::Bold => "bold",
            MarkKind::Italic => "italic",
            MarkKind::Strike => "strike",
            MarkKind::Code => "code",
            MarkKind::Link => "link",
        }
    }

    pub fn try_from_str(s: &str) -> Option<MarkKind> {
        match s {
            "bold" => Some(MarkKind::Bold),
            "italic" => Some(MarkKind::Italic),
            "strike" => Some(MarkKind::Strike),
            "code" => Some(MarkKind::Code),
            "link" => Some(MarkKind::Link),
            _ => None,
        }
    }
}

/// One styled range. `url` is only meaningful for `MarkKind::Link`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub start: usize,
    pub end: usize,
    pub kind: MarkKind,
    pub url: String,
}

impl Mark {
    pub fn covers(&self, start: usize, end: usize) -> bool {
        self.start <= start && self.end >= end
    }

    pub fn intersects(&self, start: usize, end: usize) -> bool {
        self.start < end && self.end > start
    }
}

/// One block. Belongs to exactly one page; `parent` points inside the same
/// page (`None` = top level). Siblings are ordered by `order`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: BlockId,
    pub page: PageId,
    pub parent: Option<BlockId>,
    pub order: OrderKey,
    pub kind: BlockKind,
    pub text: String,
    /// Todo checkbox state; meaningless for other kinds.
    pub checked: bool,
    /// Inline marks (M6), non-overlapping per kind.
    pub marks: Vec<Mark>,
    /// Text color of the block; `Default` inherits the theme.
    pub color: ColorKind,
    /// Row background tint behind the block; `Default` is transparent.
    pub background: ColorKind,
}

/// One page of the workspace tree.
/// `expanded` is persisted view state (Notion-like); `search_text` is NOT
/// here on purpose — it is derived from blocks, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub id: PageId,
    pub title: String,
    pub parent: Option<PageId>,
    pub order: OrderKey,
    pub favorite: bool,
    pub expanded: bool,
}

/// Full state as loaded from (or checkpointed to) storage. Vecs are in no
/// particular order; consumers sort by parent + order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedState {
    pub pages: Vec<Page>,
    pub blocks: Vec<Block>,
    pub meta: BTreeMap<String, String>,
    pub settings: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_key_midpoints() {
        let a = OrderKey(10);
        let c = OrderKey(30);
        assert_eq!(OrderKey::between(None, None), Some(OrderKey(1 << 32)));
        assert_eq!(OrderKey::between(None, Some(c)), Some(OrderKey(15)));
        assert_eq!(OrderKey::between(Some(a), None), Some(OrderKey(11)));
        assert_eq!(OrderKey::between(Some(a), Some(c)), Some(OrderKey(20)));
        // exhausted gap asks for a renumber
        assert_eq!(OrderKey::between(Some(a), Some(OrderKey(11))), None);
        assert_eq!(OrderKey::between(Some(a), Some(a)), None);
        // no room before the very first key
        assert_eq!(OrderKey::between(None, Some(OrderKey(1))), None);
        // ordering invariant holds across a chain of inserts
        let mut seq = vec![OrderKey::between(None, None).unwrap()];
        for _ in 0..8 {
            let k = OrderKey::between(Some(*seq.last().unwrap()), None).unwrap();
            assert!(k > *seq.last().unwrap());
            seq.push(k);
        }
    }

    #[test]
    fn block_kind_strings_round_trip() {
        for kind in BlockKind::ALL {
            assert_eq!(BlockKind::try_from_str(kind.as_str()), Some(kind));
        }
        assert_eq!(BlockKind::try_from_str("nope"), None);
    }

    #[test]
    fn color_slots_and_strings_round_trip() {
        for (i, kind) in ColorKind::ALL.iter().enumerate() {
            assert_eq!(kind.slot(), i as i32);
            assert_eq!(ColorKind::from_slot(i as i32), Some(*kind));
            // "" (Default) and every named color survive the DB round-trip
            assert_eq!(ColorKind::try_from_str(kind.as_str()), Some(*kind));
        }
        assert_eq!(ColorKind::from_slot(10), None);
        assert_eq!(ColorKind::try_from_str("nope"), None);
        assert_eq!(ColorKind::try_from_str(""), Some(ColorKind::Default));
    }

    #[test]
    fn ids_are_distinct_types() {
        // compile-time intent: PageId and BlockId never mix accidentally
        let p = PageId(7);
        let b = BlockId(7);
        assert_eq!(p.as_u64(), b.as_u64());
        assert_ne!(format!("{p}"), format!("{b}"));
    }
}
