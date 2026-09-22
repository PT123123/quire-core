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
id_newtype!(
    AttachmentId,
    "Stable identifier of an attachment (SPEC §三十七 批次 A)."
);

/// Sibling position. Smaller sorts first; unique among siblings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey(pub u64);

impl OrderKey {
    pub const FIRST: OrderKey = OrderKey(1 << 32);

    /// Spacing `Document::renumber_page` hands out. One insert halves a gap,
    /// so a wide stride means a renumber buys thousands of inserts instead of
    /// one — what a table needs when it adds a column (one cell per row).
    pub const STRIDE: u64 = 1 << 16;

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
    /// An embedded sub-page: `page_ref` names the child page it opens.
    Page,
    /// A link to an existing page: `page_ref` names the target, which the
    /// block does NOT own — deleting the block leaves the page alone.
    Link,
    /// A collapsible text block (SPEC §三十七 批次 B). Any kind that owns
    /// children can fold them; `Toggle` is the kind the slash and insert
    /// menus create for it, rendering the fold triangle as its marker.
    Toggle,
    /// An attached picture (SPEC §三十七 批次 A). `attachment` names the
    /// file; `text` stays empty and `img_percent` is the display width.
    Image,
    /// An attached file of any type (SPEC §三十七 批次 A). `attachment` names
    /// the bytes; `text` holds the display name, which is what the row shows
    /// when the file row itself is gone.
    File,
    /// A simple N×M grid (SPEC §三十七 批次 B). `columns` is M; the cells are
    /// child blocks in row-major order, so rows = cells / columns. Deliberately
    /// NOT a database view — schema, filters and sorting are §三十九.
    Table,
    /// One cell of a `Table`, a child block of it. Never gets its own editor
    /// row: the table's delegate projects the whole grid, so the cells stay
    /// hidden from `visible_block_indices` the same way a fold hides them.
    TableCell,
    /// 2 or 3 side-by-side columns of blocks (SPEC §三十七 批次 B). `columns`
    /// is the count; the columns themselves are child `Column` blocks, and a
    /// column's content hangs off that. Like a table it owns its subtree's
    /// rendering: the row it costs is one row, whatever it holds.
    Columns,
    /// One column of a `Columns` block: a child block that owns the column's
    /// content. Never gets its own editor row — the delegate of the `Columns`
    /// block projects it, so a column's whole subtree stays hidden.
    Column,
    /// A formula (SPEC §三十七 批次 C). `text` is the LaTeX subset source,
    /// which is what the row edits and what persists; the glyphs a row shows
    /// are derived at paint time by `core::math`, never stored.
    Math,
    /// A table of contents (SPEC §三十七 批次 C). Holds no content of its own:
    /// the page's headings *are* its body, read at projection time, so editing
    /// a heading edits the contents and nothing here can go stale.
    Toc,
    /// A link shown as a card (SPEC §三十七 批次 C). `text` is the address,
    /// which is what the row edits; the provider name on the card is derived by
    /// `core::embed` from that string and never stored. There is no iframe and
    /// no fetch behind this — §二 and §三十三 rule out a WebView and a JS
    /// runtime, so the card is the whole feature and its one action is "open
    /// this in the system browser".
    Embed,
    /// A database view (SPEC §三十九, ADR-0060). `db_ref` names the `databases`
    /// row this block draws, exactly as a `Page` block names its child page;
    /// the rows and the columns are the entity's, never the block's, because
    /// §三十九's "a record may *be* a page" has to keep a page's identity.
    ///
    /// Like a `Table` or a `Columns` block it is a **leaf** that owns no child
    /// blocks: its rows are records and its cells are values, so the projection
    /// has nothing to hide and no row-index consumer has to translate. What it
    /// owns is the entity — deleting the block deletes the `databases` row the
    /// way deleting a `Page` block deletes its child page, and a dangling
    /// `db_ref` (the entity gone, the block back through an undo) renders one
    /// muted line, "(deleted database)", exactly as a dangling `page_ref` does.
    ///
    /// The eight view layouts are *not* eight kinds: `db_views.layout` (ADR-0060
    /// / ADR-0064) is which view of this one entity is being drawn, and the six
    /// `INSERT_ITEMS` placeholders in `state.rs` are the same kinds' entry
    /// points, lit one phase at a time (D3 lights `Table view`).
    Database,
    /// A second view of another block (SPEC §四十, ADR-0052). **Owns no
    /// content of its own**: `sync_ref` names the source block, and the row
    /// draws *that* block's text and inline marks, read at projection time.
    /// `text` stays empty for the life of the block — writing into it would
    /// create two owners of one sentence, which is exactly what this design
    /// exists to avoid.
    ///
    /// A source that is gone does not blank the row: it renders a read-only
    /// placeholder, and the row stops being editable, because an edit bound to
    /// a source nobody can find would have nowhere honest to land (ADR-0052 §2).
    Synced,
}

impl BlockKind {
    pub const ALL: [BlockKind; 25] = [
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
        BlockKind::Page,
        BlockKind::Link,
        BlockKind::Toggle,
        BlockKind::Image,
        BlockKind::File,
        BlockKind::Table,
        BlockKind::TableCell,
        BlockKind::Columns,
        BlockKind::Column,
        BlockKind::Math,
        BlockKind::Toc,
        BlockKind::Embed,
        BlockKind::Synced,
        BlockKind::Database,
    ];

    /// Heading level 1..3 for a heading kind; `None` for anything else. A
    /// `Toc` lists exactly these, and its indent is this number.
    pub fn heading_level(self) -> Option<u8> {
        match self {
            BlockKind::Heading1 => Some(1),
            BlockKind::Heading2 => Some(2),
            BlockKind::Heading3 => Some(3),
            _ => None,
        }
    }

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
            BlockKind::Page => "page",
            BlockKind::Link => "link_to_page",
            BlockKind::Toggle => "toggle",
            BlockKind::Image => "image",
            BlockKind::File => "file",
            BlockKind::Table => "table",
            BlockKind::TableCell => "table_cell",
            BlockKind::Columns => "columns",
            BlockKind::Column => "column",
            BlockKind::Math => "math",
            BlockKind::Toc => "toc",
            BlockKind::Embed => "embed",
            BlockKind::Synced => "synced",
            BlockKind::Database => "database",
        }
    }

    /// Every kind reads back by name, including `Synced` — which is how the
    /// Markdown *import* contact is met without a new grammar: §四十 / ADR-0052
    /// §7 exports a mirror flattened, so there is no marker for this layer to
    /// recognise, and a file some other tool wrote with the kind spelled out
    /// becomes an unresolvable mirror rather than a load failure.
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

/// The language a code block is written in, which is what its colour keys on
/// (SPEC §三十七 批次 C). Stored as a short stable string like every other kind
/// here; `Plain` means "no colour on this block", and it is also what an
/// unknown fence folds to, so a `sql` block imported from somewhere else is not
/// a broken block but an uncoloured one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Plain,
    Rust,
    Python,
    Js,
    Ts,
    Md,
    Json,
    Bash,
}

impl Lang {
    pub const ALL: [Lang; 8] = [
        Lang::Plain,
        Lang::Rust,
        Lang::Python,
        Lang::Js,
        Lang::Ts,
        Lang::Md,
        Lang::Json,
        Lang::Bash,
    ];

    /// The string this build stores and the lexer switches on.
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Plain => "",
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Js => "js",
            Lang::Ts => "ts",
            Lang::Md => "md",
            Lang::Json => "json",
            Lang::Bash => "bash",
        }
    }

    /// What the menu shows. The names are the languages' own, not the store's.
    pub fn label(self) -> &'static str {
        match self {
            Lang::Plain => "Plain text",
            Lang::Rust => "Rust",
            Lang::Python => "Python",
            Lang::Js => "JavaScript",
            Lang::Ts => "TypeScript",
            Lang::Md => "Markdown",
            Lang::Json => "JSON",
            Lang::Bash => "Bash",
        }
    }

    /// Folds the spellings a fence or a hand-written file may use — `py`,
    /// `tsx`, `shell` — onto the one this build colours, so the picker and the
    /// importer land on the same block. `None` for a language with no lexer.
    pub fn try_from_str(s: &str) -> Option<Lang> {
        let l = s.trim().to_ascii_lowercase();
        let lang = match l.as_str() {
            "" => Lang::Plain,
            "rs" => Lang::Rust,
            "py" | "python3" => Lang::Python,
            "js" | "javascript" | "jsx" => Lang::Js,
            "ts" | "typescript" | "tsx" => Lang::Ts,
            "md" | "markdown" => Lang::Md,
            "json" | "jsonc" => Lang::Json,
            "sh" | "bash" | "shell" | "zsh" => Lang::Bash,
            other => Lang::ALL.into_iter().find(|k| k.as_str() == other)?,
        };
        Some(lang)
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
    /// `$E=mc^2$` (SPEC §三十七 批次 C). The span holds the LaTeX subset
    /// source without its delimiters, so the mark is the only place math knows
    /// about; `url` is unused.
    Math,
    /// A @page mention (SPEC §四十). The span text is the page title;
    /// `url` holds `quire://page/<id>`. `date` is unused.
    Mention,
    /// An inline date (SPEC §四十). The span text is the ISO date string;
    /// `url` is empty; `date` holds the ISO date.
    Date,
}

impl MarkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MarkKind::Bold => "bold",
            MarkKind::Italic => "italic",
            MarkKind::Strike => "strike",
            MarkKind::Code => "code",
            MarkKind::Link => "link",
            MarkKind::Math => "math",
            MarkKind::Mention => "mention",
            MarkKind::Date => "date",
        }
    }

    pub fn try_from_str(s: &str) -> Option<MarkKind> {
        match s {
            "bold" => Some(MarkKind::Bold),
            "italic" => Some(MarkKind::Italic),
            "strike" => Some(MarkKind::Strike),
            "code" => Some(MarkKind::Code),
            "link" => Some(MarkKind::Link),
            "math" => Some(MarkKind::Math),
            "mention" => Some(MarkKind::Mention),
            "date" => Some(MarkKind::Date),
            _ => None,
        }
    }
}

/// One styled range. `url` is only meaningful for `MarkKind::Link` and
/// `MarkKind::Mention`; `date` is only meaningful for `MarkKind::Date`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub start: usize,
    pub end: usize,
    pub kind: MarkKind,
    pub url: String,
    /// Only used for `MarkKind::Date` (ISO date string, e.g. "2026-09-22").
    pub date: Option<String>,
}

impl Mark {
    pub fn covers(&self, start: usize, end: usize) -> bool {
        self.start <= start && self.end >= end
    }

    pub fn intersects(&self, start: usize, end: usize) -> bool {
        self.start < end && self.end > start
    }

    /// The text that goes into the **one** payload column the `marks` table
    /// has (`url`, ADR-0050). A link and a mention put their address there; a
    /// date has no address, so its ISO string travels there instead — that
    /// column is a text payload, and this method is the only place that knows
    /// which kind puts what in it. Without it a date mark would load back with
    /// its date nowhere: the table has no second column to hold it.
    pub fn stored_payload(&self) -> &str {
        match self.kind {
            MarkKind::Date => self.date.as_deref().unwrap_or(""),
            _ => &self.url,
        }
    }

    /// The inverse of `stored_payload`: the payload column comes back and each
    /// kind's own field takes it. The round trip is pinned by a test, because
    /// "reads back as itself" is the whole contract of a storage shape.
    pub fn from_stored(start: usize, end: usize, kind: MarkKind, payload: String) -> Mark {
        let is_date = kind == MarkKind::Date;
        Mark {
            start,
            end,
            kind,
            url: if is_date { String::new() } else { payload.clone() },
            date: if is_date {
                (!payload.is_empty()).then_some(payload)
            } else {
                None
            },
        }
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
    /// The page a `Page` block opens (a child page the block owns).
    /// Meaningless for every other kind; `None` renders as a missing page.
    pub page_ref: Option<PageId>,
    /// Folded: this block's whole subtree is hidden from the editor rows
    /// (SPEC §三十七). Meaningless for a childless block, where it is still
    /// legal to store — the row simply has nothing to hide.
    pub folded: bool,
    /// The picture an `Image` block shows (SPEC §三十七 批次 A). `None` for
    /// every other kind; an `Image` block whose attachment is gone renders as
    /// a missing file rather than an empty row.
    pub attachment: Option<AttachmentId>,
    /// Display width of an `Image` block, in percent of the editor column
    /// (25 / 50 / 100). Meaningless for other kinds, where it stays 100.
    pub img_percent: u16,
    /// Column count of a `Table` block (SPEC §三十七 批次 B) or a `Columns`
    /// block. `0` means "neither": a table's cell count is always a multiple
    /// of it, so its row count is derived rather than stored, and a columns
    /// block's `Column` children are counted the same way.
    pub columns: u16,
    /// The language a `Code` block is written in (SPEC §三十七 批次 C), which
    /// is only ever read as the key for its colour. Meaningless for other kinds,
    /// where it stays `Plain` — a colour on a paragraph is `color`/`background`.
    pub lang: Lang,
    /// The database a `Database` block draws (SPEC §三十九, ADR-0060), the
    /// shape `page_ref` gave a `Page` block and for the same reason: the entity
    /// has to be reachable from the block without the block *being* it, because
    /// a record may itself be a page and a page's data may never be derived.
    /// Meaningless for every other kind, where it stays `None`.
    ///
    /// `None` on a `Database` block and a `Some` pointing at a deleted row are
    /// two different things and only the second one has a word for it: `None`
    /// is a block whose entity has not been written yet (the two are created in
    /// one batch — `Command::MakeDatabase` — so it is a state only a torn file
    /// or a hand-edit produces), while a dangling id is ADR-0060's
    /// "(deleted database)": the entity is gone and the block is back.
    ///
    /// **Not a foreign key**, for the same reason `page_ref` is not: the entity
    /// is deleted by the change that drops the block (ADR-0060), and a block
    /// whose ref dangles is a *state* the renderer has a word for, not a
    /// failure the load reports.
    pub db_ref: Option<crate::core::database::DatabaseId>,
    /// The source block a `Synced` block mirrors (SPEC §四十, ADR-0052).
    /// `None` for every other kind; on a `Synced` block it means either "no
    /// source was picked yet" or "the source is gone" — the row can tell the
    /// two apart only by trying, and both read as the same read-only
    /// placeholder, which is the honest answer in either case.
    ///
    /// **Not a foreign key.** Nothing cascades: deleting the source leaves the
    /// mirror in place and visible, and deleting the mirror leaves the source
    /// untouched. A cascade here would mean "removing one view destroys the
    /// content", which is the one outcome ADR-0052 refuses above all others.
    pub sync_ref: Option<BlockId>,
}

/// One file that lives next to the database (SPEC §三十七 批次 A, §十八's
/// reserved `attachments` table). The row is the reference; the bytes are on
/// disk, and `thumb` is the raster the editor actually loads — a picture
/// never reaches the UI at its original size (§二十二's memory budget).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: AttachmentId,
    /// Name shown to the user: the picked file's stem, not the stored name.
    pub name: String,
    /// File name inside the attachments folder, as stored.
    pub file: String,
    /// Downscaled copy of `file`, or `""` when `file` is already small enough.
    pub thumb: String,
    pub mime: String,
    pub bytes: i64,
    /// Original pixel size, 0 when unknown (a file we cannot decode).
    pub width: u32,
    pub height: u32,
}

/// The typeface one page's document tier is set to (SPEC §三十八 "页面版式").
/// A page property, never a block property: the blocks store characters and
/// this says which family draws them, so `Default` is the empty string and the
/// whole type scale stays in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PageFont {
    #[default]
    Default,
    Serif,
    Mono,
}

impl PageFont {
    pub const ALL: [PageFont; 3] = [PageFont::Default, PageFont::Serif, PageFont::Mono];

    /// The string the database holds. `""` = the app's default stack, which is
    /// also what every page written before this column existed means.
    pub fn as_str(self) -> &'static str {
        match self {
            PageFont::Default => "",
            PageFont::Serif => "serif",
            PageFont::Mono => "mono",
        }
    }

    /// What the style menu shows.
    pub fn label(self) -> &'static str {
        match self {
            PageFont::Default => "Default",
            PageFont::Serif => "Serif",
            PageFont::Mono => "Monospace",
        }
    }

    /// An unreadable spelling is no font, and the caller folds it to `Default`
    /// — a page must not fail to open because of one string.
    pub fn try_from_str(s: &str) -> Option<PageFont> {
        PageFont::ALL.into_iter().find(|f| f.as_str() == s.trim())
    }

    /// The index the editor's `page-font` integer answers to.
    pub fn slot(self) -> i32 {
        match self {
            PageFont::Default => 0,
            PageFont::Serif => 1,
            PageFont::Mono => 2,
        }
    }
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
    /// Page appearance (SPEC §三十八): which family the document tier uses,
    /// and the two layout switches. Both are one column each in `pages`, and
    /// neither reaches the blocks.
    pub font: PageFont,
    pub full_width: bool,
    pub small_text: bool,
    /// The page's own icon (SPEC §三十八 "图标与封面") — the emoji itself, not
    /// an index into a catalogue, so a page keeps its icon when the picker's
    /// list changes. Empty means unset, and the sidebar then shows
    /// [`icon::initial`] of the title instead.
    pub icon: String,
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
    fn page_font_strings_round_trip() {
        for font in PageFont::ALL {
            assert_eq!(PageFont::try_from_str(font.as_str()), Some(font));
        }
        assert_eq!(PageFont::try_from_str("comic sans"), None);
        assert_eq!(PageFont::default(), PageFont::Default);
        assert_eq!(PageFont::Default.as_str(), "");
        // the menu slot and the store string say the same order
        for (index, font) in PageFont::ALL.iter().enumerate() {
            assert_eq!(font.slot() as usize, index);
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
    fn lang_strings_round_trip_and_aliases_fold() {
        for lang in Lang::ALL {
            assert_eq!(Lang::try_from_str(lang.as_str()), Some(lang));
        }
        assert_eq!(Lang::try_from_str("PY"), Some(Lang::Python));
        assert_eq!(Lang::try_from_str(" tsx "), Some(Lang::Ts));
        assert_eq!(Lang::try_from_str("shell"), Some(Lang::Bash));
        assert_eq!(Lang::try_from_str(""), Some(Lang::Plain));
        // A language with no lexer is not a language the store keeps.
        assert_eq!(Lang::try_from_str("sql"), None);
        assert!(!Lang::ALL.iter().any(|l| l.label().is_empty()));
    }

    #[test]
    fn ids_are_distinct_types() {        // compile-time intent: PageId and BlockId never mix accidentally
        let p = PageId(7);
        let b = BlockId(7);
        assert_eq!(p.as_u64(), b.as_u64());
        assert_ne!(format!("{p}"), format!("{b}"));
    }
}
