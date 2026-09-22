// Editor commands (SPEC §十四): every semantic edit becomes a Command,
// planned against the document into a forward/revert `Entry`. The revert
// list is what undo applies — never a database re-read.

use std::collections::{HashMap, HashSet};

use super::database::{
    CellValue, DatabaseDraft, Property, PropertyId, Record, RecordId, View, ViewId,
};
use super::document::{Document, Entry};
use super::history::History;
use super::persistence::Change;
use super::types::{
    Attachment, Block, BlockId, BlockKind, ColorKind, Lang, Mark, MarkKind, OrderKey, Page, PageId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Debounced typing: replace the block's whole text (one undo step per
    /// pause). Byte-exact text, no positions involved.
    ReplaceText { id: BlockId, text: String },
    /// Enter: `[0..caret]` stays, `[caret..]` moves into a new block after
    /// this one (same kind, unchecked).
    SplitBlock { id: BlockId, caret: usize },
    /// Backspace at caret 0: merge this block into the previous one.
    MergeBackward { id: BlockId },
    DeleteBlock { id: BlockId },
    /// Block of `kind` with initial `text` directly after `id`.
    InsertBlockAfter { id: BlockId, kind: BlockKind, text: String },
    /// Block of `kind` at the end of the page, for a page that has no row to
    /// anchor on. Every other insert speaks of "after this block", so an empty
    /// page had no way in: this is its front door.
    AppendBlock { kind: BlockKind, text: String },
    /// Deep copy of one block (flat model: no subtree) right after it.
    DuplicateBlock { id: BlockId },
    SetBlockType { id: BlockId, kind: BlockKind },
    ToggleTodoChecked { id: BlockId },
    /// Move one slot up (-1) / down (+1) among the page's blocks.
    MoveBlock { id: BlockId, delta: i32 },
    /// Drag-reorder: land `id` directly above flat position `index`
    /// (0..=len) of the page's display order. Parent never changes; the
    /// landing must not split another block's subtree.
    MoveBlockTo { id: BlockId, index: i32 },
    /// Toggle an inline mark over `[start..end]` (byte offsets): a same-kind
    /// mark covering the range is removed, otherwise intersecting same-kind
    /// marks are replaced by one new mark (M6).
    ToggleMark { id: BlockId, start: usize, end: usize, kind: MarkKind, url: String },
    /// Tab on a list item: nest it under the previous list item (depth 1 max).
    IndentList { id: BlockId },
    /// Shift+Tab on a nested list item: promote it back to top level,
    /// landing after its parent's whole subtree.
    OutdentList { id: BlockId },
    /// Block-level color pair (text + row background).
    SetBlockColor { id: BlockId, color: ColorKind, background: ColorKind },
    /// Move a block (and its whole subtree) to the end of another page.
    /// The target page must differ from the block's current one.
    MoveBlockToPage { id: BlockId, page: PageId },
    /// Fold / unfold one block: its whole subtree stops producing editor rows
    /// (SPEC §三十七). Undoable, unlike the sidebar's page expand.
    ToggleFold { id: BlockId },
    /// Picture block right after `id`, pointing at `attachment` — whose bytes
    /// are already on disk by the time this is planned (SPEC §三十七 批次 A).
    /// The "+" menu and block paste, which mean "an image appears here".
    InsertImage { id: BlockId, attachment: Attachment },
    /// The same, for a file of any type: a row that names and sizes the bytes
    /// rather than drawing them (SPEC §三十七 批次 A).
    InsertFile { id: BlockId, attachment: Attachment },
    /// This block *becomes* the picture (slash "/" and Turn into — the two
    /// menus that convert the block they were opened on). The block keeps its
    /// id, so its children and its place in the page survive.
    SetBlockImage { id: BlockId, attachment: Attachment },
    /// This block becomes a file, under the same rules as `SetBlockImage`.
    SetBlockFile { id: BlockId, attachment: Attachment },
    /// Display width of an `Image` block, in percent of the editor column.
    SetImageWidth { id: BlockId, percent: u16 },
    /// The language a `Code` block is coloured as (SPEC §三十七 批次 C). A
    /// colour choice and not a content one: the characters never move, so the
    /// undo of it cannot disagree with anything that was typed since.
    SetCodeLang { id: BlockId, lang: Lang },
    /// One empty row at index `row` (`0..=rows`; `rows` appends at the bottom).
    TableAddRow { id: BlockId, row: usize },
    /// One empty column at index `col` (`0..=columns`; `columns` appends).
    TableAddColumn { id: BlockId, col: usize },
    /// Delete row `row`. Refused on the only row: the block the user is
    /// pointing at must not vanish because they removed a row.
    TableDeleteRow { id: BlockId, row: usize },
    /// Delete column `col`, refused on the only column for the same reason.
    TableDeleteColumn { id: BlockId, col: usize },
    /// One more column on a `Columns` block, empty, at the right end; refused
    /// at three (SPEC §三十七 批次 B says 2 / 3 栏).
    ColumnsAddColumn { id: BlockId },
    /// The `Columns` block's last column, folded into the one before it: its
    /// blocks keep their order keys and only change parent, so the reflow
    /// reads as "the columns got narrower" rather than as a deletion. Refused
    /// at two columns.
    ColumnsDeleteColumn { id: BlockId },
    /// One empty paragraph at the end of a `Column` box. A box is not a line
    /// the caret can land on, so the last block in it going away leaves
    /// something nothing can click into (SPEC §三十七 批次 B).
    ColumnsAddBlock { id: BlockId },

    // ─── SPEC §三十九 Database (Track 3, D3) ─────────────────────────────────
    //
    // The five commands the drawn layer adds. Two properties hold for all of
    // them and are worth stating once:
    //
    // * **The plan is a pure function of the command.** `plan` sees a
    //   `Document` (blocks) and nothing else — no SQL, no ids it can allocate
    //   for a table that is not `blocks` — so every id, every old value and
    //   every width travels *in* the command, allocated or read by the caller
    //   that can. `Command::InsertImage` set the precedent: the caller that has
    //   the attachment table is the caller that picks the attachment id.
    // * **One command, one `Entry`, one Ctrl+Z.** §三十九 requires both delete
    //   directions (a record, and the page a record is the face of) to be one
    //   undo step, and that is exactly what a change list is.
    /// Turn a line into a database block, and create the entity, its `title`
    /// column and its first view **in the same batch** (ADR-0060/0061: a
    /// database that cannot be drawn is one no path may create, so the four rows
    /// that make one appear together).
    ///
    /// The `draft` carries the three rows with their ids already allocated
    /// (`Database::new` → `title_property` → `first_view`), because the plan
    /// layer can allocate block ids and nothing else. Both the "Turn into" menu
    /// and the slash and "+" menus route here rather than through
    /// `SetBlockType`, which is why `SetBlockType { kind: Database }` is
    /// **refused** below: a caller that got there would create a block with no
    /// entity, and a block with no entity is a database that draws nothing.
    MakeDatabase { id: BlockId, draft: DatabaseDraft },
    /// One new column, at the end of the schema (`ord` past the last one). The
    /// `property` travels whole — name, kind, config, id and ord — because the
    /// plan layer has no `db_properties` table to read `ord` from, exactly as
    /// `MakeDatabase` carries its own three rows.
    ///
    /// This is what makes the drawn table more than a title column: without it
    /// a database made by the app has exactly one column forever, and no cell
    /// of any of D3's four inline editors could ever be reached. The command is
    /// the *storage* half only — editing a column's name, kind or options after
    /// the fact is `PropertyRenamed` / `PropertyKindSet`, whose UI is D5's.
    AddDatabaseProperty { block: BlockId, property: Property },
    /// One new row at `ord` (the end of the listing, which is what the view's
    /// "new row" line means). `record` is allocated by the caller — the store's
    /// ids are its own table's, and ADR-0067 means the app never loads them all
    /// to find the highest.
    ///
    /// The row is **bare** (ADR-0063's lazy page): no page is created, so a
    /// thousand-row import creates no pages, and the page arrives when someone
    /// opens the row.
    AddDatabaseRecord {
        block: BlockId,
        record: RecordId,
        ord: OrderKey,
    },
    /// Delete one row: its values, the record, and — when it is page-backed —
    /// the page it owns. `record`, `values` and `page` are read by the caller
    /// (the plan layer cannot read `db_values` or `pages`), which is what makes
    /// the undo exact instead of a reconstruction.
    ///
    /// The whole `Record` travels rather than an id, because the undo needs its
    /// `ord` and its page pointer and neither is derivable: an undo that put the
    /// row back at the end of the listing would be a different row.
    ///
    /// The forward order is ADR-0063's `[values, record, page?]` and the revert
    /// is its exact reverse (`[page?, record, values]`), because `apply` is one
    /// transaction with foreign keys **on**: a value row cannot outlive its
    /// record on the way out, and a record cannot name a page that does not
    /// exist yet on the way back. Deleting the *page* first (the sidebar's path,
    /// or a parent page's recursive delete) is SQL's `ON DELETE CASCADE` and not
    /// this command — both ends are the same state, which is the property D1's
    /// test pins.
    DeleteDatabaseRecord {
        block: BlockId,
        record: Record,
        values: Vec<(PropertyId, CellValue)>,
        /// The page row to write back when `record.page` is `Some`: the title,
        /// the parent and the appearance all have to come back, and a rebuild
        /// from the id alone would invent them.
        page: Option<Page>,
    },
    /// One cell, in ADR-0062's stored shape. `from` is the value as it is stored
    /// right now (a point read the caller made); `to` is `CellValue::Empty` when
    /// the edit cleared the cell, which **removes the row** rather than writing
    /// a blank — absence is this design's one representation of empty, so a
    /// number cell is never `0`.
    ///
    /// A write aimed at a derived kind (`created time` / `last edited time`) is
    /// a caller error with no sensible meaning: the plan refuses it rather than
    /// recording a `CellSet` that no read path would ever consult (ADR-0068).
    SetDatabaseCell {
        block: BlockId,
        record: RecordId,
        property: PropertyId,
        from: CellValue,
        to: CellValue,
    },
    /// Replace a view's rules document whole (ADR-0064). D3 writes it for two
    /// things — a column's width and a column being hidden — and both are edits
    /// of `columns`/`widths` inside the document, so the caller passes the whole
    /// new text. Replaced rather than merged in storage for ADR-0064's reason:
    /// the document is the view's own truth, and merging it in two places is how
    /// its shape ends up defined twice.
    SetDatabaseViewDefinition {
        block: BlockId,
        view: ViewId,
        from: String,
        to: String,
    },
    /// One new view of the database a block draws — D5's switcher `+`: a row
    /// in `db_views` with its own layout, born with an empty rules document
    /// (ADR-0060: a view is `db_views.layout`, not a block kind; ADR-0064: the
    /// rules are the document, and a new view has none yet).
    ///
    /// The row travels whole — id, name, layout, ord — because the plan layer
    /// can allocate none of them (`MakeDatabase`'s rule, applied to the one
    /// table whose rows the app keeps out of memory). The id is the caller's
    /// watermark (ADR-0072), the name defaults to the layout's label and is
    /// renameable later, and the ord is past the last view so a new view lands
    /// at the switcher's end.
    ///
    /// The inverse is the view alone: a view nobody has edited yet holds no
    /// document and owns no rows, so `ViewDeleted` is the whole of the undo.
    AddDatabaseView { block: BlockId, view: View },
}

/// The grid shape the "+", the slash menu and "Turn into" hand out. Three
/// columns because the first row is the Markdown header row, and two rows give
/// it something to head.
pub const TABLE_DEFAULT_COLUMNS: u16 = 3;
pub const TABLE_DEFAULT_ROWS: u16 = 2;

/// The layout the slash menu and "Turn into" hand out, and the widest the
/// hover strip reaches. Two is the default because it is the shape that still
/// reads as two columns at a normal window width.
pub const COLUMNS_DEFAULT: u16 = 2;
pub const COLUMNS_MAX: u16 = 3;

fn is_table_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Table | BlockKind::TableCell)
}

/// The kinds whose blocks are containers rather than lines: they draw their
/// own subtree inside one editor row, so no structural editing reaches into
/// them as if they were prose (SPEC §三十七 批次 B).
fn is_container_kind(kind: BlockKind) -> bool {
    is_table_kind(kind) || is_column_kind(kind)
}

fn is_column_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Columns | BlockKind::Column)
}

/// True when the block is drawn by a container's delegate instead of by a row
/// of its own: a cell, a column box, or anything below them.
fn inside_container(doc: &Document, id: BlockId) -> bool {
    let mut parent = match doc.block(id) {
        Some(b) => b.parent,
        None => return false,
    };
    let mut guard = 0;
    while let Some(pid) = parent {
        let Some(p) = doc.block(pid) else { break };
        if is_container_kind(p.kind) {
            return true;
        }
        parent = p.parent;
        guard += 1;
        if guard >= 32 {
            break;
        }
    }
    false
}

/// A table as stored: its column count and its cells in row-major sequence.
/// Cells are child blocks, and a parent's children keep their relative order
/// in the page's display order, so filtering the page yields the grid.
struct Grid {
    slots: Vec<usize>,
    cells: Vec<Block>,
    cols: usize,
    table_columns: u16,
}

impl Grid {
    fn rows(&self) -> usize {
        self.cells.len() / self.cols
    }
}

/// Read the grid, or `None` when `id` is not a well-formed table. A grid whose
/// cell count is not a multiple of its column count is not editable: the plan
/// refuses rather than guess which row is short.
fn grid(doc: &Document, page: PageId, id: BlockId) -> Option<Grid> {
    let blocks = doc.page_blocks(page);
    let table = blocks.get(doc.index_of(page, id)?)?;
    if table.kind != BlockKind::Table || table.columns == 0 {
        return None;
    }
    let cols = table.columns as usize;
    let mut slots = Vec::new();
    let mut cells = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if b.parent == Some(id) && b.kind == BlockKind::TableCell {
            slots.push(i);
            cells.push(b.clone());
        }
    }
    if cells.len() % cols != 0 {
        return None;
    }
    Some(Grid { slots, cells, cols, table_columns: table.columns })
}

/// Page slots bounding a new cell that must sort after cell sequence position
/// `at - 1` and before position `at` (`0..=` the cell count): the block to sort
/// after, and the block to sort before (`None` only when the run ends the
/// page).
fn run_bounds(g: &Grid, tslot: usize, at: usize) -> (Option<usize>, Option<usize>) {
    let lo = if at == 0 { Some(tslot) } else { Some(g.slots[at - 1]) };
    let after_run = g.slots.last().map(|s| s + 1).unwrap_or(tslot + 1);
    (lo, Some(*g.slots.get(at).unwrap_or(&after_run)))
}

/// `n` order keys strictly between `lo` and `hi`, evenly spread. `None` when
/// the gap cannot hold that many — one midpoint per insert halves a gap, so a
/// batch of inserts needs the whole run planned at once.
fn keys_between(lo: Option<OrderKey>, hi: Option<OrderKey>, n: usize) -> Option<Vec<OrderKey>> {
    let n = n as u64;
    match (lo, hi) {
        (Some(a), Some(b)) if b.0 > a.0 + n => {
            let step = (b.0 - a.0) / (n + 1);
            (0..n).map(|i| OrderKey(a.0 + (i + 1) * step)).collect::<Vec<_>>().into()
        }
        (Some(a), None) => {
            (0..n).map(|i| a.0.checked_add(i + 1)).collect::<Option<Vec<_>>>().map(|v| v.into_iter().map(OrderKey).collect())
        }
        (None, Some(b)) if b.0 > n => {
            let step = b.0 / (n + 1);
            (0..n).map(|i| OrderKey((i + 1) * step)).collect::<Vec<_>>().into()
        }
        (None, None) => (0..n)
            .map(|i| OrderKey(OrderKey::FIRST.0 + (i + 1) * OrderKey::STRIDE))
            .collect::<Vec<_>>()
            .into(),
        _ => None,
    }
}

/// Order keys for one batch of inserts, each into its own gap between two
/// existing page slots. When a gap is too tight the page is renumbered and
/// every gap re-derived: a renumber preserves relative order, so the slot
/// indices stay valid and no key generated on the failed attempt is in the
/// document yet.
fn keys_in_gaps(doc: &mut Document, page: PageId, gaps: &[(Option<usize>, Option<usize>, usize)]) -> Option<Vec<Vec<OrderKey>>> {
    for attempt in 0..2 {
        if attempt == 1 {
            doc.renumber_page(page);
        }
        let blocks = doc.page_blocks(page);
        let at = |slot: Option<usize>| slot.and_then(|i| blocks.get(i)).map(|b| b.order);
        let mut out = Vec::with_capacity(gaps.len());
        let mut fits = true;
        for (lo, hi, n) in gaps {
            match keys_between(at(*lo), at(*hi), *n) {
                Some(keys) => out.push(keys),
                None => {
                    fits = false;
                    break;
                }
            }
        }
        if fits {
            return Some(out);
        }
    }
    None
}

/// One empty child block of `parent`, of `kind`, ordered by `order`. Both
/// container kinds build their subtree out of these: a table's cells and a
/// columns block's columns.
fn new_child(doc: &mut Document, page: PageId, parent: BlockId, order: OrderKey, kind: BlockKind) -> Block {
    Block {
        id: doc.alloc_block_id(),
        page,
        parent: Some(parent),
        order,
        kind,
        text: String::new(),
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
    }
}

/// One empty cell of table `parent`, ordered by `order`.
fn new_cell(doc: &mut Document, page: PageId, parent: BlockId, order: OrderKey) -> Block {
    new_child(doc, page, parent, order, BlockKind::TableCell)
}

/// A columns block's `Column` children, in display order. The page's block
/// list is display order, so filtering it yields them — the same reading
/// `grid` makes of a table.
fn column_blocks(doc: &Document, page: PageId, id: BlockId) -> Option<Vec<Block>> {
    let b = doc.block(id)?;
    if b.kind != BlockKind::Columns {
        return None;
    }
    Some(
        doc.page_blocks(page)
            .iter()
            .filter(|x| x.parent == Some(id) && x.kind == BlockKind::Column)
            .cloned()
            .collect(),
    )
}

/// The direct content of column `parent`: what one column of the layout shows,
/// in order. A nested block under one of these rides along inside it — the
/// delegate walks the column's whole subtree, so a re-parent is all a
/// reflow needs.
fn column_children(doc: &Document, page: PageId, column: BlockId) -> Vec<Block> {
    doc.page_blocks(page)
        .iter()
        .filter(|b| b.parent == Some(column))
        .cloned()
        .collect()
}

fn is_list_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Bullet | BlockKind::Numbered | BlockKind::Todo)
}

/// Order key for appending as the last child of `parent` on `page`: between
/// the parent's last child (or the parent itself) and the parent's next
/// top-level sibling. Renumbers the page when the gap is exhausted.
fn child_key_after_last(doc: &mut Document, page: PageId, parent: BlockId) -> Option<OrderKey> {
    let key = {
        let blocks = doc.page_blocks(page);
        let parent_order = blocks.iter().find(|b| b.id == parent)?.order;
        let last_child = blocks
            .iter()
            .filter(|b| b.parent == Some(parent))
            .map(|b| b.order)
            .max();
        OrderKey::between(Some(last_child.unwrap_or(parent_order)), None)
    };
    if key.is_some() {
        return key;
    }
    doc.renumber_page(page);
    let blocks = doc.page_blocks(page);
    let parent_order = blocks.iter().find(|b| b.id == parent)?.order;
    let last_child = blocks
        .iter()
        .filter(|b| b.parent == Some(parent))
        .map(|b| b.order)
        .max();
    OrderKey::between(Some(last_child.unwrap_or(parent_order)), None)
}

/// Snap a caret to a valid char boundary (<= given offset).
fn snap_left(text: &str, caret: usize) -> usize {
    if text.is_char_boundary(caret) {
        caret
    } else {
        (0..caret).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0)
    }
}

/// Order key for a block inserted directly after index `idx`; renumbers the
/// page when the gap between neighbors is exhausted (relative order is
/// unchanged, so the silent re-key is safe for persistence).
fn key_after(doc: &mut Document, page: PageId, idx: usize) -> Option<OrderKey> {
    let before = doc.page_blocks(page).get(idx)?.order;
    let after = doc.page_blocks(page).get(idx + 1).map(|b| b.order);
    if let Some(key) = OrderKey::between(Some(before), after) {
        return Some(key);
    }
    doc.renumber_page(page);
    let before = doc.page_blocks(page).get(idx)?.order;
    let after = doc.page_blocks(page).get(idx + 1).map(|b| b.order);
    OrderKey::between(Some(before), after)
}

/// A block and its whole subtree, in display order — which, because a
/// parent's children directly follow it, also lists every parent before its
/// descendants. A delete cascades (`Document::remove_recursive` in memory, the
/// recursive `WITH RECURSIVE` statement in storage), so undoing one has to
/// write every descendant back, not just the root it removed.
fn subtree(doc: &Document, page: PageId, root: BlockId) -> Vec<Block> {
    let blocks = doc.page_blocks(page);
    if !blocks.iter().any(|b| b.id == root) {
        return Vec::new();
    }
    let mut keep = HashSet::from([root]);
    loop {
        let added: Vec<BlockId> = blocks
            .iter()
            .filter(|b| b.parent.is_some_and(|p| keep.contains(&p)) && !keep.contains(&b.id))
            .map(|b| b.id)
            .collect();
        if added.is_empty() {
            break;
        }
        keep.extend(added);
    }
    blocks.iter().filter(|b| keep.contains(&b.id)).cloned().collect()
}

/// The two attachment kinds share their whole shape: a block that points at
/// stored bytes and takes the file name as its text. Only `kind` differs —
/// and with it the width tier, which only a picture draws.
fn insert_attachment(
    doc: &mut Document,
    page: PageId,
    id: BlockId,
    attachment: Attachment,
    kind: BlockKind,
) -> Option<Entry> {
    let idx = doc.index_of(page, id)?;
    let order = key_after(doc, page, idx)?;
    let new_id = doc.alloc_block_id();
    let aid = attachment.id;
    // The file name doubles as the block's text: it is what Markdown export
    // and Ctrl+F have to work with, since neither sees the attachment table.
    // The delegate never paints it for a picture.
    let text = attachment.name.clone();
    let new = Block {
        id: new_id,
        page,
        parent: None,
        order,
        kind,
        text,
        checked: false,
        marks: Vec::new(),
        color: ColorKind::Default,
        background: ColorKind::Default,
        page_ref: None,
        folded: false,
        attachment: Some(aid),
        img_percent: 100,
        columns: 0,
        lang: Lang::Plain,
        db_ref: None,
    };
    Some(Entry {
        apply: vec![Change::AttachmentAdded(attachment), Change::BlockInserted(new)],
        // Undo drops the reference only. The row and the file stay: redo must
        // work without touching disk, and a user who undoes a paste should not
        // lose bytes they can still find in the attachments folder.
        revert: vec![Change::BlockDeleted { id: new_id }],
    })
}

/// This block becomes the attachment: same id, so its subtree and its place in
/// the page survive; only kind, text and reference change.
fn set_block_attachment(
    doc: &mut Document,
    id: BlockId,
    attachment: Attachment,
    kind: BlockKind,
) -> Option<Entry> {
    let b = doc.block(id)?;
    let old_kind = b.kind;
    let old_text = b.text.clone();
    let old_att = b.attachment;
    // Same convention as InsertAttachment: the file name is the block text. A
    // Turn-into therefore replaces the line's words, and undo puts them back —
    // the attachment has nowhere else to keep its name.
    let text = attachment.name.clone();
    let aid = attachment.id;
    let mut apply = vec![
        Change::AttachmentAdded(attachment),
        Change::BlockTextSet { id, text },
        Change::BlockAttachmentSet {
            id,
            attachment: Some(aid),
        },
    ];
    let mut revert = vec![
        Change::BlockAttachmentSet { id, attachment: old_att },
        Change::BlockTextSet { id, text: old_text },
    ];
    if old_kind != kind {
        apply.push(Change::BlockKindSet { id, kind });
        revert.push(Change::BlockKindSet { id, kind: old_kind });
    }
    // A folded block keeps hiding its subtree as an attachment, and only a
    // Toggle draws a chevron — same rule SetBlockType applies.
    if b.folded {
        apply.push(Change::BlockFoldedSet { id, folded: false });
        revert.push(Change::BlockFoldedSet { id, folded: true });
    }
    Some(Entry { apply, revert })
}

/// Does landing at slot `p` (flat insert-above position in the CURRENT
/// order, 0..=len) keep the model well-formed for the block at `s`?
///
/// - Top-level: must not split another block's subtree — the block that
///   follows the landing slot must be top-level (children always directly
///   follow their parent's subtree in the flat order).
/// - Nested (parent P): must stay adjacent to P's sibling run, i.e. touch a
///   sibling, P itself, or the run boundary.
fn move_landing_ok(blocks: &[Block], s: usize, p: usize) -> bool {
    let q = if s < p { p - 1 } else { p };
    let orig = |r: usize| if r < s { r } else { r + 1 };
    let parent = blocks[s].parent;
    let next = if q >= blocks.len() - 1 { None } else { Some(&blocks[orig(q)]) };
    let prev = if q == 0 { None } else { Some(&blocks[orig(q - 1)]) };
    match parent {
        None => next.map_or(true, |b| b.parent.is_none()),
        Some(p_id) => {
            let prev_ok = prev.is_some_and(|b| b.parent == Some(p_id) || b.id == p_id);
            let next_ok = next.is_some_and(|b| b.parent == Some(p_id));
            prev_ok || next_ok
        }
    }
}

/// Read-only validity check for a drag landing (hover feedback must not
/// mutate the document, so this skips the order-gap/renumber step).
pub fn can_move_block_to(doc: &Document, page: PageId, id: BlockId, index: i32) -> bool {
    let Some(idx) = doc.index_of(page, id) else { return false };
    let blocks = doc.page_blocks(page);
    let Ok(p) = usize::try_from(index) else { return false };
    if p > blocks.len() || p == idx || p == idx + 1 {
        return false;
    }
    move_landing_ok(blocks, idx, p)
}

/// Plan `cmd` against the current document. Returns `None` for no-ops
/// (missing ids, exhausted edges, unchanged text). Allocates ids on `doc`
/// when the command creates blocks.
pub fn plan(doc: &mut Document, page: PageId, cmd: Command) -> Option<Entry> {
    match cmd {
        Command::ReplaceText { id, text } => {
            let old = doc.block(id)?.text.clone();
            if old == text {
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockTextSet { id, text }],
                revert: vec![Change::BlockTextSet { id, text: old }],
            })
        }

        Command::SplitBlock { id, caret } => {
            let idx = doc.index_of(page, id)?;
            let first = doc.page_blocks(page).get(idx)?.clone();
            if first.kind == BlockKind::Divider || is_container_kind(first.kind) {
                return None;
            }
            let caret = snap_left(&first.text, caret);
            let head = first.text[..caret].to_string();
            let tail = first.text[caret..].to_string();
            let order = key_after(doc, page, idx)?;
            let new_id = doc.alloc_block_id();
            let mut new = first.clone();
            new.id = new_id;
            new.order = order;
            new.text = tail;
            new.checked = false;
            // the tail carries no children, so it has nothing to hide
            new.folded = false;
            Some(Entry {
                apply: vec![
                    Change::BlockTextSet { id, text: head },
                    Change::BlockInserted(new),
                ],
                revert: vec![
                    Change::BlockDeleted { id: new_id },
                    Change::BlockTextSet { id, text: first.text },
                ],
            })
        }

        Command::MergeBackward { id } => {
            let idx = doc.index_of(page, id)?;
            let this = doc.page_blocks(page).get(idx)?.clone();
            if is_container_kind(this.kind) {
                // a cell has no block to merge into — its neighbours are grid
                // slots, not prose — and a container has no text to merge
                return None;
            }
            if idx == 0 {
                // first block: only an empty one may vanish
                if this.text.is_empty() {
                    Some(Entry {
                        apply: vec![Change::BlockDeleted { id }],
                        revert: subtree(doc, page, id)
                            .into_iter()
                            .map(Change::BlockInserted)
                            .collect(),
                    })
                } else {
                    None
                }
            } else {
                let prev = doc.page_blocks(page).get(idx - 1)?.clone();
                if prev.kind == BlockKind::Divider
                    || is_container_kind(prev.kind)
                    || prev.parent != this.parent
                {
                    // Merging stays inside one parent: the flat order puts a
                    // container's whole subtree between the container and the
                    // next line, so merging across it would hand the words to
                    // a block the row cannot show (a grid draws no text) or to
                    // a sibling of another column.
                    return None;
                }
                Some(Entry {
                    apply: vec![
                        Change::BlockTextSet { id: prev.id, text: prev.text.clone() + &this.text },
                        Change::BlockDeleted { id },
                    ],
                    revert: {
                        let mut r: Vec<Change> = subtree(doc, page, id)
                            .into_iter()
                            .map(Change::BlockInserted)
                            .collect();
                        r.push(Change::BlockTextSet { id: prev.id, text: prev.text });
                        r
                    },
                })
            }
        }

        Command::DeleteBlock { id } => {
            let idx = doc.index_of(page, id)?;
            let this = doc.page_blocks(page).get(idx)?.clone();
            if matches!(this.kind, BlockKind::TableCell | BlockKind::Column) {
                // cells die with a row or a column, and a column with its
                // layout — neither is a block the user can point at
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockDeleted { id }],
                // the delete took the subtree with it, so undo re-inserts the
                // whole of it — a table's grid, a toggle's children
                revert: subtree(doc, page, id)
                    .into_iter()
                    .map(Change::BlockInserted)
                    .collect(),
            })
        }

        Command::InsertBlockAfter { id, kind, text } => {
            if is_container_kind(kind) {
                return None; // a grid or a layout is built by SetBlockType, not by a bare kind
            }
            let idx = doc.index_of(page, id)?;
            let anchor = doc.page_blocks(page).get(idx)?.clone();
            // The anchor decides where the new block lands and who owns it. A
            // container's row is its whole subtree, so "+" on a layout means "a
            // block after the layout" — not one wedged between it and its
            // boxes, which would break the flat order everything else reads.
            // A block inside a container keeps its container: a paste at a
            // caret in a box belongs to that box.
            let (slot, parent) = match anchor.kind {
                BlockKind::Columns | BlockKind::Table => {
                    (doc.index_of(page, subtree(doc, page, id).last()?.id)?, None)
                }
                BlockKind::Column | BlockKind::TableCell => {
                    return None; // a slot of a container, not a row of its own
                }
                _ => (idx, anchor.parent),
            };
            let order = key_after(doc, page, slot)?;
            let new_id = doc.alloc_block_id();
            let new = Block {
                id: new_id,
                page,
                parent,
                order,
                kind,
                text,
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
            };
            Some(Entry {
                apply: vec![Change::BlockInserted(new)],
                revert: vec![Change::BlockDeleted { id: new_id }],
            })
        }
        Command::AppendBlock { kind, text } => {
            if is_container_kind(kind) {
                return None; // same rule as InsertBlockAfter
            }
            // Appending after a container's last subtree block lands outside
            // it, because a container's children are ordered right behind it.
            let last = doc.page_blocks(page).last().map(|b| b.order);
            let order = OrderKey::between(last, None)?;
            let new_id = doc.alloc_block_id();
            let new = Block {
                id: new_id,
                page,
                parent: None,
                order,
                kind,
                text,
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
            };
            Some(Entry {
                apply: vec![Change::BlockInserted(new)],
                revert: vec![Change::BlockDeleted { id: new_id }],
            })
        }

        Command::InsertImage { id, attachment } => {
            insert_attachment(doc, page, id, attachment, BlockKind::Image)
        }

        Command::InsertFile { id, attachment } => {
            insert_attachment(doc, page, id, attachment, BlockKind::File)
        }

        Command::SetBlockImage { id, attachment } => {
            set_block_attachment(doc, id, attachment, BlockKind::Image)
        }

        Command::SetBlockFile { id, attachment } => {
            set_block_attachment(doc, id, attachment, BlockKind::File)
        }

        Command::SetImageWidth { id, percent } => {
            let old = doc.block(id)?.img_percent;
            if old == percent {
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockImageWidthSet { id, percent }],
                revert: vec![Change::BlockImageWidthSet { id, percent: old }],
            })
        }

        Command::SetCodeLang { id, lang } => {
            let old = doc.block(id)?.lang;
            if old == lang {
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockLangSet { id, lang }],
                revert: vec![Change::BlockLangSet { id, lang: old }],
            })
        }

        Command::TableAddRow { id, row } => {
            let tslot = doc.index_of(page, id)?;
            let g = grid(doc, page, id)?;
            if row > g.rows() {
                return None;
            }
            let (lo, hi) = run_bounds(&g, tslot, row * g.cols);
            let keys = keys_in_gaps(doc, page, &[(lo, hi, g.cols)])?;
            let mut cells = Vec::new();
            for k in &keys[0] {
                cells.push(new_cell(doc, page, id, *k));
            }
            Some(Entry {
                apply: cells.iter().cloned().map(Change::BlockInserted).collect(),
                revert: cells.iter().map(|c| Change::BlockDeleted { id: c.id }).collect(),
            })
        }

        Command::TableAddColumn { id, col } => {
            let tslot = doc.index_of(page, id)?;
            let g = grid(doc, page, id)?;
            let (rows, cols) = (g.rows(), g.cols);
            if rows == 0 || col > cols {
                return None;
            }
            // one cell per row, each into its own gap, so the whole batch is
            // keyed before any id is allocated
            let after_run = *g.slots.last().unwrap_or(&tslot) + 1;
            let gaps: Vec<(Option<usize>, Option<usize>, usize)> = (0..rows)
                .map(|r| {
                    let lo = if col > 0 {
                        g.slots[r * cols + col - 1]
                    } else if r > 0 {
                        g.slots[r * cols - 1]
                    } else {
                        tslot
                    };
                    let hi = if col < cols {
                        g.slots[r * cols + col]
                    } else if r + 1 < rows {
                        g.slots[(r + 1) * cols]
                    } else {
                        after_run
                    };
                    (Some(lo), Some(hi), 1)
                })
                .collect();
            let keys = keys_in_gaps(doc, page, &gaps)?;
            let mut cells = Vec::new();
            for k in keys.iter().flatten() {
                cells.push(new_cell(doc, page, id, *k));
            }
            let mut apply = vec![Change::BlockColumnsSet { id, columns: (cols + 1) as u16 }];
            apply.extend(cells.iter().cloned().map(Change::BlockInserted));
            let mut revert: Vec<Change> =
                cells.iter().map(|c| Change::BlockDeleted { id: c.id }).collect();
            revert.push(Change::BlockColumnsSet { id, columns: g.table_columns });
            Some(Entry { apply, revert })
        }

        Command::TableDeleteRow { id, row } => {
            let g = grid(doc, page, id)?;
            if g.rows() <= 1 || row >= g.rows() {
                return None; // never the last row: the table itself stays
            }
            let doomed: Vec<Block> = g.cells[row * g.cols..(row + 1) * g.cols].to_vec();
            Some(Entry {
                apply: doomed.iter().map(|c| Change::BlockDeleted { id: c.id }).collect(),
                revert: doomed.iter().cloned().map(Change::BlockInserted).collect(),
            })
        }

        Command::TableDeleteColumn { id, col } => {
            let g = grid(doc, page, id)?;
            let (rows, cols) = (g.rows(), g.cols);
            if cols <= 1 || col >= cols {
                return None;
            }
            let doomed: Vec<Block> =
                (0..rows).map(|r| g.cells[r * cols + col].clone()).collect();
            let mut apply = vec![Change::BlockColumnsSet { id, columns: (cols - 1) as u16 }];
            apply.extend(doomed.iter().map(|c| Change::BlockDeleted { id: c.id }));
            let mut revert: Vec<Change> =
                doomed.iter().cloned().map(Change::BlockInserted).collect();
            revert.push(Change::BlockColumnsSet { id, columns: g.table_columns });
            Some(Entry { apply, revert })
        }

        Command::ColumnsAddColumn { id } => {
            let cols = column_blocks(doc, page, id)?;
            if cols.len() >= COLUMNS_MAX as usize {
                return None; // three is as wide as the layout goes
            }
            // the new column and its first line sort after the layout's whole
            // subtree, which is where a column's siblings end
            let end = doc.index_of(page, subtree(doc, page, id).last()?.id)?;
            let keys =
                keys_in_gaps(doc, page, &[(Some(end), Some(end + 1), 2)])?;
            let column = new_child(doc, page, id, keys[0][0], BlockKind::Column);
            let line = new_child(doc, page, column.id, keys[0][1], BlockKind::Paragraph);
            let (cid, lid) = (column.id, line.id);
            Some(Entry {
                apply: vec![
                    Change::BlockColumnsSet { id, columns: (cols.len() + 1) as u16 },
                    Change::BlockInserted(column),
                    Change::BlockInserted(line),
                ],
                revert: vec![
                    Change::BlockDeleted { id: lid },
                    Change::BlockDeleted { id: cid },
                    Change::BlockColumnsSet { id, columns: cols.len() as u16 },
                ],
            })
        }

        Command::ColumnsDeleteColumn { id } => {
            let cols = column_blocks(doc, page, id)?;
            if cols.len() <= COLUMNS_DEFAULT as usize {
                return None; // never below two: the layout itself stays
            }
            let doomed = cols.last()?;
            let target = cols.get(cols.len() - 2)?;
            // The last column's blocks move into the one before it and keep
            // their order keys: they already sort after its own content, so
            // the reflow needs no re-keying — and nothing is deleted but the
            // shape.
            let mut apply = Vec::new();
            let mut revert = vec![Change::BlockInserted(doomed.clone())];
            for item in column_children(doc, page, doomed.id) {
                apply.push(Change::BlockMoved {
                    id: item.id,
                    parent: Some(target.id),
                    order: item.order,
                });
                revert.push(Change::BlockMoved {
                    id: item.id,
                    parent: Some(doomed.id),
                    order: item.order,
                });
            }
            apply.push(Change::BlockColumnsSet { id, columns: (cols.len() - 1) as u16 });
            apply.push(Change::BlockDeleted { id: doomed.id });
            revert.push(Change::BlockColumnsSet { id, columns: cols.len() as u16 });
            Some(Entry { apply, revert })
        }

        Command::ColumnsAddBlock { id } => {
            let this = doc.block(id)?;
            if this.kind != BlockKind::Column {
                return None; // only a box has boxes to fill
            }
            // after the box's whole subtree and before whatever follows it, so
            // the line reads as the box's last block rather than as the next
            // box's first
            let end = doc.index_of(page, subtree(doc, page, id).last()?.id)?;
            let keys = keys_in_gaps(doc, page, &[(Some(end), Some(end + 1), 1)])?;
            let line = new_child(doc, page, id, keys[0][0], BlockKind::Paragraph);
            let lid = line.id;
            Some(Entry {
                apply: vec![Change::BlockInserted(line)],
                revert: vec![Change::BlockDeleted { id: lid }],
            })
        }

        Command::DuplicateBlock { id } => {
            let idx = doc.index_of(page, id)?;
            let src = doc.page_blocks(page).get(idx)?.clone();
            // A columns block IS its content: copying only the shell would put
            // an empty layout next to a full one, so the copy carries the whole
            // subtree with freshly minted ids and its parent links remapped.
            if src.kind == BlockKind::Columns {
                let group = subtree(doc, page, id);
                let land_after = doc.index_of(page, group.last()?.id)?;
                let keys = keys_in_gaps(
                    doc,
                    page,
                    &[(Some(land_after), Some(land_after + 1), group.len())],
                )?;
                let fresh: Vec<BlockId> = group.iter().map(|_| doc.alloc_block_id()).collect();
                let remap: HashMap<BlockId, BlockId> =
                    group.iter().zip(&fresh).map(|(b, n)| (b.id, *n)).collect();
                let mut apply = Vec::with_capacity(group.len());
                for (src, (new_id, order)) in
                    group.iter().zip(fresh.iter().zip(&keys[0]))
                {
                    let parent = if src.id == id {
                        src.parent // the root keeps the parent it had
                    } else {
                        Some(*remap.get(&src.parent?)?)
                    };
                    let mut copy = src.clone();
                    copy.id = *new_id;
                    copy.order = *order;
                    copy.parent = parent;
                    // a copy of the layout root must not start out hiding a
                    // subtree the copy does have — the fold is a view state
                    if src.id == id {
                        copy.folded = false;
                    }
                    apply.push(Change::BlockInserted(copy));
                }
                let copy_id = *fresh.first()?;
                return Some(Entry {
                    // one delete is enough: it cascades to the copy's subtree
                    apply,
                    revert: vec![Change::BlockDeleted { id: copy_id }],
                });
            }
            // A table's cells are part of it: the copy has to carry its own
            // grid, and it lands after the source's whole run so the two stay
            // readable as two tables. A grid that cannot be read must not be
            // copied at all — a cell-less table would open broken.
            let g = if src.kind == BlockKind::Table {
                Some(grid(doc, page, id)?)
            } else {
                None
            };
            let land_after = g.as_ref().and_then(|g| g.slots.last().copied()).unwrap_or(idx);
            let kids = g.map(|g| g.cells).unwrap_or_default();
            let keys =
                keys_in_gaps(doc, page, &[(Some(land_after), Some(land_after + 1), kids.len() + 1)])?;
            let keys = &keys[0];
            let new_id = doc.alloc_block_id();
            let mut copy = src.clone();
            copy.id = new_id;
            copy.order = keys[0];
            // a duplicate is flat (no subtree): it must not start out hiding
            // children that were never copied
            copy.folded = false;
            let mut apply = vec![Change::BlockInserted(copy)];
            for (src_cell, k) in kids.iter().zip(keys.iter().skip(1)) {
                let mut cell = src_cell.clone();
                cell.id = doc.alloc_block_id();
                cell.parent = Some(new_id);
                cell.order = *k;
                apply.push(Change::BlockInserted(cell));
            }
            Some(Entry {
                // one delete is enough: storage cascades to the copy's cells
                apply,
                revert: vec![Change::BlockDeleted { id: new_id }],
            })
        }

        Command::SetBlockType { id, kind } => {
            let b = doc.block(id)?;
            let old = b.kind;
            let folded = b.folded;
            let text = b.text.clone();
            let columns = b.columns;
            if old == kind
                || matches!(kind, BlockKind::TableCell | BlockKind::Column)
                || matches!(old, BlockKind::TableCell | BlockKind::Column)
            {
                // a cell's kind belongs to its grid and a column's to its
                // layout, not to the Turn-into menu
                return None;
            }
            if kind == BlockKind::Database {
                // A `Database` block is only half of a database: the other half
                // is the entity, its title column and its first view, and their
                // ids are not this layer's to allocate. `MakeDatabase` is the one
                // command that makes the block and those three rows in one batch
                // (ADR-0060), and both menus route through it — so a caller that
                // arrives here has skipped the only path that works, and a block
                // with no entity draws nothing at all. Refusing is the honest
                // answer; doing it halfway is how a database loses its rows.
                return None;
            }
            if (kind == BlockKind::Table || kind == BlockKind::Columns)
                && (doc.page_blocks(page).iter().any(|x| x.parent == Some(id))
                    || inside_container(doc, id))
            {
                // the block already owns blocks: as a container its children
                // would vanish behind the grid or the layout, so the
                // conversion is refused. So is a block inside a container:
                // the row's delegate has no room for a second one.
                return None;
            }
            let mut apply = vec![Change::BlockKindSet { id, kind }];
            let mut revert = vec![Change::BlockKindSet { id, kind: old }];
            // Only a Toggle draws a chevron, so a folded block of another
            // kind would hide its subtree with no way back: turning one into
            // a plain kind re-opens it (and undo re-closes it).
            if folded && kind != BlockKind::Toggle {
                apply.push(Change::BlockFoldedSet { id, folded: false });
                revert.push(Change::BlockFoldedSet { id, folded: true });
            }
            if old == BlockKind::Table && columns > 0 {
                // Flattening a grid keeps the words: the cells become
                // paragraphs, which the row projection shows again.
                let g = grid(doc, page, id)?;
                apply.push(Change::BlockColumnsSet { id, columns: 0 });
                revert.push(Change::BlockColumnsSet { id, columns });
                for c in &g.cells {
                    apply.push(Change::BlockKindSet { id: c.id, kind: BlockKind::Paragraph });
                    revert.push(Change::BlockKindSet { id: c.id, kind: BlockKind::TableCell });
                }
            }
            if old == BlockKind::Columns && columns > 0 {
                // Flattening a layout keeps the words and drops the shape: the
                // columns' blocks move up under the line being flattened, which
                // leaves them in the same display order as its children, and
                // the now-empty column containers go away with the shape.
                let cols = column_blocks(doc, page, id)?;
                apply.push(Change::BlockColumnsSet { id, columns: 0 });
                revert.push(Change::BlockColumnsSet { id, columns });
                for c in &cols {
                    // the container comes back before its content is re-keyed
                    // onto it, which is the order storage's parent check needs
                    revert.push(Change::BlockInserted(c.clone()));
                    for item in column_children(doc, page, c.id) {
                        apply.push(Change::BlockMoved {
                            id: item.id,
                            parent: Some(id),
                            order: item.order,
                        });
                        revert.push(Change::BlockMoved {
                            id: item.id,
                            parent: Some(c.id),
                            order: item.order,
                        });
                    }
                    apply.push(Change::BlockDeleted { id: c.id });
                }
            }
            if kind == BlockKind::Columns {
                // The block's own words become the first line of the first
                // column: a layout draws no text of its own, and losing a line
                // to a menu pick is not an acceptable trade for the columns.
                let idx = doc.index_of(page, id)?;
                let n = COLUMNS_DEFAULT as usize;
                // one column, and one line inside it, per slot
                let keys = keys_in_gaps(doc, page, &[(Some(idx), Some(idx + 1), 2 * n)])?;
                let mut created = Vec::new();
                for (i, pair) in keys[0].chunks(2).enumerate() {
                    let column = new_child(doc, page, id, pair[0], BlockKind::Column);
                    let mut line = new_child(doc, page, column.id, pair[1], BlockKind::Paragraph);
                    if i == 0 {
                        line.text = text.clone();
                    }
                    created.push(column.id);
                    created.push(line.id);
                    apply.push(Change::BlockInserted(column));
                    apply.push(Change::BlockInserted(line));
                }
                apply.push(Change::BlockColumnsSet { id, columns: COLUMNS_DEFAULT });
                if !text.is_empty() {
                    apply.push(Change::BlockTextSet { id, text: String::new() });
                }
                // reversed, so a line dies before the column holding it: a
                // delete that found no row is a storage error, not a no-op
                for c in created.into_iter().rev() {
                    revert.push(Change::BlockDeleted { id: c });
                }
                revert.push(Change::BlockColumnsSet { id, columns: 0 });
                if !text.is_empty() {
                    revert.push(Change::BlockTextSet { id, text: text.clone() });
                }
            }
            if kind == BlockKind::Table {
                // The block's own words become the top-left cell: a table
                // block draws no text, and losing a line to a menu pick is not
                // an acceptable trade for the grid.
                let idx = doc.index_of(page, id)?;
                let cols = TABLE_DEFAULT_COLUMNS as usize;
                let total = cols * TABLE_DEFAULT_ROWS as usize;
                let keys = keys_in_gaps(doc, page, &[(Some(idx), Some(idx + 1), total)])?;
                let mut created = Vec::new();
                for (i, k) in keys[0].iter().enumerate() {
                    let mut cell = new_cell(doc, page, id, *k);
                    if i == 0 {
                        cell.text = text.clone();
                    }
                    created.push(cell.clone());
                    apply.push(Change::BlockInserted(cell));
                }
                apply.push(Change::BlockColumnsSet { id, columns: TABLE_DEFAULT_COLUMNS });
                if !text.is_empty() {
                    apply.push(Change::BlockTextSet { id, text: String::new() });
                }
                for c in created.into_iter().rev() {
                    revert.push(Change::BlockDeleted { id: c.id });
                }
                revert.push(Change::BlockColumnsSet { id, columns: 0 });
                if !text.is_empty() {
                    revert.push(Change::BlockTextSet { id, text });
                }
            }
            Some(Entry { apply, revert })
        }

        // ─── SPEC §三十九 Database ───────────────────────────────────────────
        Command::MakeDatabase { id, draft } => {
            let b = doc.block(id)?;
            let old = b.kind;
            // The same two refusals `SetBlockType` makes, for the same reasons:
            // a cell's kind belongs to its grid, and a block that already owns
            // blocks cannot become a leaf (its children would vanish behind the
            // view with no way back).
            if matches!(old, BlockKind::TableCell | BlockKind::Column)
                || inside_container(doc, id)
                || b.db_ref.is_some()
            {
                return None;
            }
            if doc.page_blocks(page).iter().any(|x| x.parent == Some(id)) {
                return None;
            }
            let db = draft.database.id;
            Some(Entry {
                // Order matters and is not stylistic: the entity exists before
                // the pointer names it (the column has no foreign key, but the
                // *renderer* would draw a dangling ref for one frame otherwise),
                // and the title column and the first view are written with it —
                // ADR-0061's "no path may create a database that cannot be
                // drawn", which is why they travel in the same batch.
                apply: vec![
                    Change::DatabaseCreated(draft.database.clone()),
                    Change::PropertyAdded(draft.title.clone()),
                    Change::ViewAdded(draft.view.clone()),
                    Change::BlockDbRefSet {
                        id,
                        db: Some(db),
                    },
                    Change::BlockKindSet {
                        id,
                        kind: BlockKind::Database,
                    },
                ],
                // Three changes, not five: deleting the entity cascades its
                // columns and its views away (ADR-0061/0064), so the properties
                // and the views do not have to be taken back one by one. The
                // pointer is cleared first — in reverse, that is the last thing
                // done and the first thing undone, so the block is never left
                // pointing at a row that is on its way out.
                revert: vec![
                    Change::BlockKindSet { id, kind: old },
                    Change::BlockDbRefSet { id, db: None },
                    Change::DatabaseDeleted { id: db },
                ],
            })
        }

        Command::AddDatabaseProperty { block, property } => {
            let b = doc.block(block)?;
            let db = b.db_ref?;
            // The column belongs to the entity this *block* draws, so a property
            // aimed at another database is refused rather than written: the two
            // ids travel separately in the command and could disagree, and the
            // result would be a column no view of this block can ever show.
            if property.db != db {
                return None;
            }
            Some(Entry {
                apply: vec![Change::PropertyAdded(property.clone())],
                // Deleting the column cascades its values away (ADR-0062), so
                // the inverse is the column alone — and the value rows come back
                // only if the undo that puts them back runs after this one,
                // which is why the store's delete and this share a transaction
                // boundary and nothing else has to be listed here.
                revert: vec![Change::PropertyDeleted { id: property.id }],
            })
        }

        Command::AddDatabaseRecord { block, record, ord } => {
            let b = doc.block(block)?;
            let db = b.db_ref?;
            Some(Entry {
                apply: vec![Change::RecordCreated(Record::bare(record, db, ord))],
                // The inverse is the record alone: a row that was just created
                // has no values and no page (it is born bare, ADR-0063), so
                // undoing its creation cannot leave anything behind.
                revert: vec![Change::RecordDeleted { id: record }],
            })
        }

        Command::DeleteDatabaseRecord {
            block,
            record,
            values,
            page,
        } => {
            let b = doc.block(block)?;
            if b.db_ref.is_none() {
                return None;
            }
            let id = record.id;
            let mut apply: Vec<Change> = Vec::with_capacity(values.len() + 2);
            let mut revert: Vec<Change> = Vec::with_capacity(values.len() + 2);
            // The forward order is ADR-0063's, and the revert is its reverse.
            // `apply` is one transaction with foreign keys on, so the two orders
            // are not interchangeable: a value row cannot outlive its record on
            // the way out, and on the way back the record cannot name a page
            // that has not been written yet.
            for (property, value) in values {
                // "Remove this value" is `CellSet { value: Empty }` and not a
                // delete change of its own: ADR-0062 makes the absent row *the*
                // representation of empty, so one variant covers writing a value
                // and clearing one, and the undo is the same variant with the
                // value it had.
                apply.push(Change::CellSet {
                    record: id,
                    property: property,
                    value: CellValue::Empty,
                });
                revert.push(Change::CellSet {
                    record: id,
                    property: property,
                    value: value.clone(),
                });
            }
            apply.push(Change::RecordDeleted { id });
            if let Some(page) = page {
                apply.push(Change::PageDeleted { id: page.id });
                revert.push(Change::PageCreated(page.clone()));
            }
            revert.push(Change::RecordCreated(record));
            Some(Entry { apply, revert })
        }

        Command::SetDatabaseCell {
            block,
            record,
            property,
            from,
            to,
        } => {
            let b = doc.block(block)?;
            if b.db_ref.is_none() || from == to {
                // The same write twice is not an undo step: a click that lands on
                // the value a checkbox already has must not push a history entry.
                return None;
            }
            Some(Entry {
                apply: vec![Change::CellSet {
                    record,
                    property,
                    value: to.clone(),
                }],
                revert: vec![Change::CellSet {
                    record,
                    property,
                    value: from,
                }],
            })
        }

        Command::SetDatabaseViewDefinition {
            block,
            view,
            from,
            to,
        } => {
            let b = doc.block(block)?;
            if b.db_ref.is_none() || from == to {
                return None;
            }
            Some(Entry {
                apply: vec![Change::ViewDefinitionSet {
                    id: view,
                    definition: to,
                }],
                revert: vec![Change::ViewDefinitionSet {
                    id: view,
                    definition: from,
                }],
            })
        }

        Command::AddDatabaseView { block, view } => {
            let b = doc.block(block)?;
            let db = b.db_ref?;
            // The view belongs to the entity this *block* draws — the same
            // refusal `AddDatabaseProperty` makes, for the same reason: the two
            // ids travel separately and could disagree, and the result would be
            // a view no tab of this block can ever show.
            if view.db != db {
                return None;
            }
            Some(Entry {
                apply: vec![Change::ViewAdded(view.clone())],
                revert: vec![Change::ViewDeleted { id: view.id }],
            })
        }

        Command::ToggleTodoChecked { id } => {
            let b = doc.block(id)?;
            if b.kind != BlockKind::Todo {
                return None;
            }
            let checked = !b.checked;
            Some(Entry {
                apply: vec![Change::BlockCheckedSet { id, checked }],
                revert: vec![Change::BlockCheckedSet { id, checked: !checked }],
            })
        }

        Command::ToggleFold { id } => {
            let b = doc.block(id)?;
            let folded = !b.folded;
            Some(Entry {
                apply: vec![Change::BlockFoldedSet { id, folded }],
                revert: vec![Change::BlockFoldedSet { id, folded: !folded }],
            })
        }

        Command::ToggleMark { id, start, end, kind, url } => {
            let b = doc.block(id)?;
            let (start, end) = (start.min(end), end.max(start));
            let end = end.min(b.text.len());
            let start = start.min(end);
            if start == end {
                return None;
            }
            let mut new_marks: Vec<Mark> = Vec::new();
            let mut was_covered = false;
            for m in &b.marks {
                if m.kind == kind {
                    if m.covers(start, end) {
                        was_covered = true; // toggle off: drop it
                    }
                    if m.intersects(start, end) {
                        continue; // replaced by the new range
                    }
                }
                new_marks.push(m.clone());
            }
            if !was_covered {
                new_marks.push(Mark { start, end, kind, url });
                new_marks.sort_by_key(|m| (m.start, m.end));
            }
            if new_marks == b.marks {
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockMarksSet { id, marks: new_marks }],
                revert: vec![Change::BlockMarksSet { id, marks: b.marks.clone() }],
            })
        }

        Command::IndentList { id } => {
            let idx = doc.index_of(page, id)?;
            let (this, prev) = {
                let blocks = doc.page_blocks(page);
                let this = blocks.get(idx)?.clone();
                let prev = if idx == 0 { None } else { blocks.get(idx - 1) };
                let prev = match prev {
                    Some(p) => p.clone(),
                    None => return None,
                };
                (this, prev)
            };
            if !is_list_kind(this.kind) || this.parent.is_some() {
                return None; // depth 1 max, list kinds only
            }
            if !is_list_kind(prev.kind) || prev.parent.is_some() {
                return None; // only nest under a top-level list item
            }
            let order = child_key_after_last(doc, page, prev.id)?;
            let mut new = this.clone();
            new.parent = Some(prev.id);
            new.order = order;
            Some(Entry {
                apply: vec![Change::BlockMoved { id, parent: Some(prev.id), order }],
                revert: vec![Change::BlockMoved { id, parent: None, order: this.order }],
            })
        }

        Command::OutdentList { id } => {
            let idx = doc.index_of(page, id)?;
            let this = {
                let blocks = doc.page_blocks(page);
                blocks.get(idx)?.clone()
            };
            let parent_id = this.parent?;
            if doc.block(parent_id)?.kind == BlockKind::Column {
                return None; // a column is the layout's boundary: what is in it
                             // leaves through the layout, not through Shift+Tab
            }
            // land after the parent's whole subtree, before the parent's
            // next top-level sibling (renumber when the gap is exhausted)
            let order = {
                let bounds = {
                    let blocks = doc.page_blocks(page);
                    let parent_order = blocks.iter().find(|b| b.id == parent_id)?.order;
                    let mut max_subtree = parent_order;
                    let mut stack = vec![parent_id];
                    while let Some(pid) = stack.pop() {
                        for b in blocks.iter().filter(|b| b.parent == Some(pid)) {
                            if b.order > max_subtree {
                                max_subtree = b.order;
                            }
                            stack.push(b.id);
                        }
                    }
                    let next_top = blocks
                        .iter()
                        .filter(|b| b.parent.is_none() && b.order > parent_order)
                        .map(|b| b.order)
                        .min();
                    (Some(max_subtree), next_top)
                };
                if let Some(k) = OrderKey::between(bounds.0, bounds.1) {
                    k
                } else {
                    doc.renumber_page(page);
                    let blocks = doc.page_blocks(page);
                    let parent_order = blocks.iter().find(|b| b.id == parent_id)?.order;
                    let mut max_subtree = parent_order;
                    let mut stack = vec![parent_id];
                    while let Some(pid) = stack.pop() {
                        for b in blocks.iter().filter(|b| b.parent == Some(pid)) {
                            if b.order > max_subtree {
                                max_subtree = b.order;
                            }
                            stack.push(b.id);
                        }
                    }
                    let next_top = blocks
                        .iter()
                        .filter(|b| b.parent.is_none() && b.order > parent_order)
                        .map(|b| b.order)
                        .min();
                    OrderKey::between(Some(max_subtree), next_top)?
                }
            };
            let mut new = this.clone();
            new.parent = None;
            new.order = order;
            Some(Entry {
                apply: vec![Change::BlockMoved { id, parent: None, order }],
                revert: vec![Change::BlockMoved { id, parent: Some(parent_id), order: this.order }],
            })
        }

        Command::MoveBlock { id, delta } => {
            let idx = doc.index_of(page, id)?;
            let blocks = doc.page_blocks(page);
            let this = blocks.get(idx)?.clone();
            if this.kind == BlockKind::TableCell {
                // its siblings are grid slots, so a swap would shuffle two
                // cells instead of moving anything
                return None;
            }
            let target = i32::try_from(idx).ok()? + delta;
            if target < 0 {
                return None;
            }
            let neighbor_idx = usize::try_from(target).ok()?;
            let neighbor = blocks.get(neighbor_idx)?.clone();
            if this.parent != neighbor.parent {
                return None; // reordering stays within a sibling run
            }
            // swapping keys keeps both unique and flips the order; the
            // parent is re-set to the same value so nested items stay nested
            Some(Entry {
                apply: vec![
                    Change::BlockMoved { id: this.id, parent: this.parent, order: neighbor.order },
                    Change::BlockMoved { id: neighbor.id, parent: this.parent, order: this.order },
                ],
                revert: vec![
                    Change::BlockMoved { id: this.id, parent: this.parent, order: this.order },
                    Change::BlockMoved { id: neighbor.id, parent: this.parent, order: neighbor.order },
                ],
            })
        }

        Command::MoveBlockTo { id, index } => {
            let idx = doc.index_of(page, id)?;
            let blocks = doc.page_blocks(page);
            let len = blocks.len();
            let Ok(p) = usize::try_from(index) else { return None };
            if p > len {
                return None;
            }
            if p == idx || p == idx + 1 {
                return None; // dropping on itself (or right below it): no-op
            }
            if !move_landing_ok(blocks, idx, p) {
                return None;
            }
            let this = blocks[idx].clone();
            // insertion slot `q` in the source-removed list; neighbors are
            // mapped back to original indices via orig()
            let q = if idx < p { p - 1 } else { p };
            let orig = |r: usize| if r < idx { r } else { r + 1 };
            let neighbors = |blocks: &[Block]| {
                (
                    if q == 0 { None } else { Some(blocks[orig(q - 1)].order) },
                    if q >= len - 1 { None } else { Some(blocks[orig(q)].order) },
                )
            };
            let (prev, next) = neighbors(blocks);
            let order = match OrderKey::between(prev, next) {
                Some(k) => k,
                None => {
                    doc.renumber_page(page); // relative order preserved: orig() stays valid
                    let blocks = doc.page_blocks(page);
                    let (prev, next) = neighbors(blocks);
                    OrderKey::between(prev, next)?
                }
            };
            Some(Entry {
                apply: vec![Change::BlockMoved { id, parent: this.parent, order }],
                revert: vec![Change::BlockMoved { id, parent: this.parent, order: this.order }],
            })
        }

        Command::SetBlockColor { id, color, background } => {
            let b = doc.block(id)?;
            if b.color == color && b.background == background {
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockColorSet { id, color, background }],
                revert: vec![Change::BlockColorSet { id, color: b.color, background: b.background }],
            })
        }

        Command::MoveBlockToPage { id, page: target } => {
            if target == page {
                return None; // same-page moves are MoveBlockTo's job
            }
            let idx = doc.index_of(page, id)?;
            let this = doc.page_blocks(page).get(idx)?.clone();
            // capture the whole subtree in display order (children follow
            // their parent in the flat order)
            let blocks = doc.page_blocks(page);
            let mut subtree: Vec<Block> = vec![this.clone()];
            loop {
                let before = subtree.len();
                for b in blocks {
                    if subtree.iter().any(|s| Some(s.id) == b.parent)
                        && !subtree.iter().any(|s| s.id == b.id)
                    {
                        subtree.push(b.clone());
                    }
                }
                if subtree.len() == before {
                    break;
                }
            }
            // the root lands as the last top-level block of the target page
            let append_key = |doc: &mut Document, page: PageId| -> Option<OrderKey> {
                let max = doc
                    .page_blocks(page)
                    .iter()
                    .filter(|b| b.parent.is_none())
                    .map(|b| b.order)
                    .max();
                match OrderKey::between(max, None) {
                    Some(k) => Some(k),
                    None => {
                        doc.renumber_page(page);
                        let max = doc
                            .page_blocks(page)
                            .iter()
                            .filter(|b| b.parent.is_none())
                            .map(|b| b.order)
                            .max();
                        OrderKey::between(max, None)
                    }
                }
            };
            let root_order = append_key(doc, target)?;
            let mut apply = Vec::new();
            let mut revert = Vec::new();
            for (i, b) in subtree.iter().enumerate() {
                let (parent, order) = if i == 0 {
                    (None, root_order)
                } else {
                    (b.parent, b.order)
                };
                apply.push(Change::BlockMovedToPage {
                    id: b.id,
                    page: target,
                    parent,
                    order,
                });
                revert.push(Change::BlockMovedToPage {
                    id: b.id,
                    page: b.page,
                    parent: b.parent,
                    order: b.order,
                });
            }
            revert.reverse();
            Some(Entry { apply, revert })
        }
    }
}

/// Plan, apply, and record. Returns the forward changes (the persistence
/// feed, M3).
pub fn exec(doc: &mut Document, hist: &mut History, page: PageId, cmd: Command) -> Option<Vec<Change>> {
    let entry = plan(doc, page, cmd)?;
    doc.apply(&entry.apply);
    hist.push(page, entry.clone());
    Some(entry.apply)
}

/// Plan several commands against the pre-state and apply them as ONE
/// history entry (single undo step) — for compound UI actions like the
/// slash menu (replace text + set type). Commands must be independent
/// (none may depend on the applied result of an earlier one). No-op
/// commands (plan → None) are skipped; an all-no-op list is a no-op.
pub fn exec_all(
    doc: &mut Document,
    hist: &mut History,
    page: PageId,
    cmds: Vec<Command>,
) -> Option<Vec<Change>> {
    let mut apply = Vec::new();
    let mut revert = Vec::new();
    for cmd in cmds {
        let Some(entry) = plan(doc, page, cmd) else { continue };
        apply.extend(entry.apply);
        revert.extend(entry.revert);
    }
    if apply.is_empty() {
        return None;
    }
    revert.reverse();
    doc.apply(&apply);
    let entry = Entry { apply: apply.clone(), revert };
    hist.push(page, entry);
    Some(apply)
}

/// Undo the last command on `page`; returns the changes now in effect (the
/// persistence feed).
pub fn undo(doc: &mut Document, hist: &mut History, page: PageId) -> Option<Vec<Change>> {
    let entry = hist.undo(page)?;
    doc.apply(&entry.revert);
    Some(entry.revert)
}

/// Redo the last undone command on `page`.
pub fn redo(doc: &mut Document, hist: &mut History, page: PageId) -> Option<Vec<Change>> {
    let entry = hist.redo(page)?;
    doc.apply(&entry.apply);
    Some(entry.apply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::AttachmentId;
    use crate::core::document::Document;
    

    fn setup() -> (Document, History, PageId, Vec<BlockId>) {
        let mut doc = Document::new(1000);
        let page = PageId(1);
        let texts = ["first", "second", "third"];
        let mut ids = Vec::new();
        let mut prev: Option<OrderKey> = None;
        for t in texts {
            let order = OrderKey::between(prev, None).unwrap();
            let id = doc.alloc_block_id();
            let mut v = doc.page_blocks(page).to_vec();
            v.push(Block {
                id,
                page,
                parent: None,
                order,
                kind: BlockKind::Paragraph,
                text: t.into(),
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
            });
            doc.set_page_blocks(page, v);
            prev = Some(order);
            ids.push(id);
        }
        (doc, History::default(), page, ids)
    }

    #[test]
    fn split_creates_tail_block_and_undo_restores() {
        let (mut doc, mut hist, page, ids) = setup();
        let f = ids[0];
        // "fi|rst"
        let caret = 2;
        let changes = exec(&mut doc, &mut hist, page, Command::SplitBlock { id: f, caret }).unwrap();
        assert_eq!(changes.len(), 2);
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks[0].text, "fi");
        assert_eq!(blocks[1].text, "rst");
        assert_eq!(blocks[1].kind, BlockKind::Paragraph);
        assert!(blocks[0].order < blocks[1].order);

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 3);
        assert_eq!(doc.page_blocks(page)[0].text, "first");
        redo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 4);
        assert_eq!(doc.page_blocks(page)[1].text, "rst");
    }

    #[test]
    fn merge_backward_joins_previous_and_undo_restores() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(&mut doc, &mut hist, page, Command::MergeBackward { id: ids[1] }).unwrap();
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "firstsecond");

        undo(&mut doc, &mut hist, page);
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].text, "first");
        assert_eq!(blocks[1].text, "second");
    }

    #[test]
    fn merge_backward_first_block_only_when_empty() {
        let (mut doc, mut hist, page, ids) = setup();
        assert!(plan(&mut doc, page, Command::MergeBackward { id: ids[0] }).is_none());
        exec(&mut doc, &mut hist, page, Command::ReplaceText { id: ids[0], text: String::new() }).unwrap();
        exec(&mut doc, &mut hist, page, Command::MergeBackward { id: ids[0] }).unwrap();
        assert_eq!(doc.page_blocks(page).len(), 2);
    }

    #[test]
    fn move_block_swaps_with_neighbor() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(&mut doc, &mut hist, page, Command::MoveBlock { id: ids[1], delta: -1 }).unwrap();
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["second", "first", "third"]);
        // edge: moving the first block up is a no-op
        assert!(plan(&mut doc, page, Command::MoveBlock { id: ids[1], delta: -1 }).is_none());
    }

    #[test]
    fn move_to_lands_at_flat_position_and_undoes() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(&mut doc, &mut hist, page, Command::MoveBlockTo { id: ids[2], index: 0 }).unwrap();
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["third", "first", "second"]);
        undo(&mut doc, &mut hist, page);
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    #[test]
    fn move_to_edge_cases() {
        let (mut doc, mut hist, page, ids) = setup();
        // dropping on itself or directly below itself: no-op
        assert!(plan(&mut doc, page, Command::MoveBlockTo { id: ids[1], index: 1 }).is_none());
        assert!(plan(&mut doc, page, Command::MoveBlockTo { id: ids[1], index: 2 }).is_none());
        // append after the last block
        exec(&mut doc, &mut hist, page, Command::MoveBlockTo { id: ids[0], index: 3 }).unwrap();
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["second", "third", "first"]);
    }

    #[test]
    fn move_to_keeps_parent_structure() {
        let mut doc = Document::new(1000);
        let mut hist = History::default();
        let page = PageId(1);
        let p1 = doc.alloc_block_id();
        let mk = |id: BlockId, text: &str, order: u64, parent: Option<BlockId>| Block {
            id,
            page,
            parent,
            order: OrderKey(order),
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
        };
        let c1 = doc.alloc_block_id();
        let c2 = doc.alloc_block_id();
        let p2 = doc.alloc_block_id();
        doc.set_page_blocks(
            page,
            vec![
                mk(p1, "p1", 10, None),
                mk(c1, "c1", 12, Some(p1)),
                mk(c2, "c2", 14, Some(p1)),
                mk(p2, "p2", 16, None),
            ],
        );
        let ids: Vec<BlockId> = doc.page_blocks(page).iter().map(|b| b.id).collect();

        // a top-level block cannot land inside p1's subtree (between p1 and c1)
        assert!(plan(&mut doc, page, Command::MoveBlockTo { id: ids[3], index: 1 }).is_none());
        // ... nor between the two children
        assert!(plan(&mut doc, page, Command::MoveBlockTo { id: ids[3], index: 2 }).is_none());
        // ... but the top of the page is fine
        assert!(plan(&mut doc, page, Command::MoveBlockTo { id: ids[3], index: 0 }).is_some());
        // a nested item cannot leave its sibling run
        assert!(plan(&mut doc, page, Command::MoveBlockTo { id: ids[1], index: 0 }).is_none());
        // ... but can reorder within it (c2 above c1, keeping its parent)
        exec(&mut doc, &mut hist, page, Command::MoveBlockTo { id: ids[2], index: 1 }).unwrap();
        let rows = doc.page_blocks(page);
        let texts: Vec<&str> = rows.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["p1", "c2", "c1", "p2"]);
        assert_eq!(rows[1].parent, Some(p1));
        // hover-side validity check agrees, read-only
        assert!(can_move_block_to(&doc, page, ids[3], 0));
        assert!(!can_move_block_to(&doc, page, ids[3], 1));
        assert!(!can_move_block_to(&doc, page, ids[3], 4)); // p == s+1
    }

    #[test]
    fn move_to_renumbers_when_gap_exhausted() {
        let mut doc = Document::new(1000);
        let mut hist = History::default();
        let page = PageId(1);
        let mk = |id: BlockId, text: &str, order: u64| Block {
            id,
            page,
            parent: None,
            order: OrderKey(order),
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
        };
        let a = doc.alloc_block_id();
        let b = doc.alloc_block_id();
        let c = doc.alloc_block_id();
        doc.set_page_blocks(
            page,
            vec![mk(a, "a", 1), mk(b, "b", 2), mk(c, "c", 3)],
        );
        let ids: Vec<BlockId> = doc.page_blocks(page).iter().map(|b| b.id).collect();
        // a(1) above c(3): the b..c gap is exhausted, the silent renumber kicks in
        exec(&mut doc, &mut hist, page, Command::MoveBlockTo { id: ids[0], index: 2 }).unwrap();
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["b", "a", "c"]);
    }

    #[test]
    fn replace_text_noop_when_equal_and_history_caps() {
        let (mut doc, mut hist, page, ids) = setup();
        assert!(plan(&mut doc, page, Command::ReplaceText { id: ids[0], text: "first".into() }).is_none());
        for i in 0..200 {
            exec(&mut doc, &mut hist, page, Command::ReplaceText { id: ids[0], text: format!("t{i}") }).unwrap();
        }
        for _ in 0..200 {
            if undo(&mut doc, &mut hist, page).is_none() {
                break;
            }
        }
        // capped history: at most 100 undo steps, never panics
        assert!(undo(&mut doc, &mut hist, page).is_none());
    }

    #[test]
    fn duplicate_copies_block_after_original() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(&mut doc, &mut hist, page, Command::DuplicateBlock { id: ids[0] }).unwrap();
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["first", "first", "second", "third"]);
        undo(&mut doc, &mut hist, page);
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    #[test]
    fn exec_all_is_one_undo_step() {
        let (mut doc, mut hist, page, ids) = setup();
        exec_all(
            &mut doc,
            &mut hist,
            page,
            vec![
                Command::ReplaceText { id: ids[0], text: "retitled".into() },
                Command::SetBlockType { id: ids[0], kind: BlockKind::Heading1 },
            ],
        )
        .unwrap();
        let b = doc.block(ids[0]).unwrap();
        assert_eq!(b.text, "retitled");
        assert_eq!(b.kind, BlockKind::Heading1);
        // one undo reverts BOTH
        undo(&mut doc, &mut hist, page);
        let b = doc.block(ids[0]).unwrap();
        assert_eq!(b.text, "first");
        assert_eq!(b.kind, BlockKind::Paragraph);
    }

    #[test]
    fn insert_block_after_carries_text() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::InsertBlockAfter {
                id: ids[2],
                kind: BlockKind::Code,
                text: "println!(\"hi\");".into(),
            },
        )
        .unwrap();
        let last = doc.page_blocks(page).last().unwrap();
        assert_eq!(last.kind, BlockKind::Code);
        assert_eq!(last.text, "println!(\"hi\");");
    }

    #[test]
    fn toggle_mark_adds_then_removes() {
        let (mut doc, mut hist, page, ids) = setup();
        let f = ids[0]; // "first": bytes 0..5
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::ToggleMark { id: f, start: 0, end: 5, kind: MarkKind::Bold, url: String::new() },
        )
        .unwrap();
        assert_eq!(doc.block(f).unwrap().marks.len(), 1);
        // toggle again removes it
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::ToggleMark { id: f, start: 0, end: 5, kind: MarkKind::Bold, url: String::new() },
        )
        .unwrap();
        assert!(doc.block(f).unwrap().marks.is_empty());
    }

    #[test]
    fn toggle_mark_replaces_intersecting_same_kind() {
        let (mut doc, mut hist, page, ids) = setup();
        let f = ids[0];
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::ToggleMark { id: f, start: 0, end: 2, kind: MarkKind::Bold, url: String::new() },
        )
        .unwrap();
        // overlapping range replaces the old mark instead of stacking
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::ToggleMark { id: f, start: 1, end: 5, kind: MarkKind::Bold, url: String::new() },
        )
        .unwrap();
        let marks = doc.block(f).unwrap().marks.clone();
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].start, marks[0].end), (1, 5));
        // different kinds coexist
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::ToggleMark { id: f, start: 1, end: 5, kind: MarkKind::Italic, url: String::new() },
        )
        .unwrap();
        assert_eq!(doc.block(f).unwrap().marks.len(), 2);
    }

    #[test]
    fn indent_nests_under_previous_list_item() {
        let (mut doc, mut hist, page, ids) = setup();
        // make all three bullets
        for id in &ids {
            exec(&mut doc, &mut hist, page, Command::SetBlockType { id: *id, kind: BlockKind::Bullet }).unwrap();
        }
        exec(&mut doc, &mut hist, page, Command::IndentList { id: ids[1] }).unwrap();
        let nested_id = {
            let blocks = doc.page_blocks(page);
            assert_eq!(blocks[1].parent, Some(ids[0]));
            assert!(blocks[0].order < blocks[1].order);
            blocks[1].id
        };
        // depth 1 max: indenting an already-nested item is a no-op
        assert!(plan(&mut doc, page, Command::IndentList { id: nested_id }).is_none());
        // indenting under a paragraph is a no-op
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[0], kind: BlockKind::Paragraph }).unwrap();
        assert!(plan(&mut doc, page, Command::IndentList { id: ids[1] }).is_none());
    }

    #[test]
    fn outdent_lands_after_the_parent_subtree() {
        let (mut doc, mut hist, page, ids) = setup();
        for id in &ids {
            exec(&mut doc, &mut hist, page, Command::SetBlockType { id: *id, kind: BlockKind::Bullet }).unwrap();
        }
        exec(&mut doc, &mut hist, page, Command::IndentList { id: ids[1] }).unwrap();
        exec(&mut doc, &mut hist, page, Command::OutdentList { id: ids[1] }).unwrap();
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks[1].parent, None);
        // between bullet1 and bullet3, still in display order
        assert!(blocks[0].order < blocks[1].order && blocks[1].order < blocks[2].order);
        // outdent a top-level block: no-op
        assert!(plan(&mut doc, page, Command::OutdentList { id: ids[1] }).is_none());
    }

    #[test]
    fn move_stays_within_sibling_run() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[0], kind: BlockKind::Bullet }).unwrap();
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[1], kind: BlockKind::Bullet }).unwrap();
        exec(&mut doc, &mut hist, page, Command::IndentList { id: ids[1] }).unwrap();
        // ids[1] is now nested under ids[0]; swapping with ids[2] (different
        // parents) must be refused
        assert!(plan(&mut doc, page, Command::MoveBlock { id: ids[1], delta: 1 }).is_none());
    }

    #[test]
    fn per_page_histories_are_independent() {
        let (mut doc, mut hist, page, ids) = setup();
        let page2 = PageId(2);
        exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: ids[0] }).unwrap();
        assert!(undo(&mut doc, &mut hist, page2).is_none(), "page2 has no history");
        assert!(undo(&mut doc, &mut hist, page).is_some());
    }

    #[test]
    fn set_block_color_round_trips_through_undo() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockColor {
                id: ids[0],
                color: ColorKind::Red,
                background: ColorKind::Blue,
            },
        )
        .unwrap();
        let b = doc.block(ids[0]).unwrap();
        assert_eq!((b.color, b.background), (ColorKind::Red, ColorKind::Blue));
        // the same pick is a no-op
        assert!(
            plan(
                &mut doc,
                page,
                Command::SetBlockColor {
                    id: ids[0],
                    color: ColorKind::Red,
                    background: ColorKind::Blue,
                }
            )
            .is_none()
        );
        undo(&mut doc, &mut hist, page);
        let b = doc.block(ids[0]).unwrap();
        assert_eq!(
            (b.color, b.background),
            (ColorKind::Default, ColorKind::Default)
        );
    }

    #[test]
    fn move_block_to_page_carries_subtree_and_undo_restores() {
        let mut doc = Document::new(1000);
        let mut hist = History::default();
        let source = PageId(1);
        let target = PageId(2);
        let mk = |doc: &mut Document, page: PageId, text: &str, order: u64, parent: Option<BlockId>| Block {
            id: doc.alloc_block_id(),
            page,
            parent,
            order: OrderKey(order),
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
        };
        let p = mk(&mut doc, source, "p", 10, None);
        let c = mk(&mut doc, source, "c", 12, Some(p.id));
        let t = mk(&mut doc, target, "t", 10, None);
        doc.set_page_blocks(source, vec![p.clone(), c.clone()]);
        doc.set_page_blocks(target, vec![t.clone()]);

        exec(
            &mut doc,
            &mut hist,
            source,
            Command::MoveBlockToPage { id: p.id, page: target },
        )
        .unwrap();
        // the source page is empty; the subtree sits at the end of the target
        assert!(doc.page_blocks(source).is_empty());
        let rows = doc.page_blocks(target);
        let texts: Vec<&str> = rows.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["t", "p", "c"]);
        assert_eq!(rows[1].parent, None);
        assert_eq!(rows[2].parent, Some(p.id));
        assert!(rows[1].order > rows[0].order);

        undo(&mut doc, &mut hist, source);
        let rows = doc.page_blocks(source);
        let texts: Vec<&str> = rows.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["p", "c"]);
        assert_eq!(rows[0].order, p.order);
        assert_eq!(rows[1].parent, Some(p.id));
        assert_eq!(doc.page_blocks(target)[0].text, "t");
    }

    #[test]
    fn move_block_to_page_refuses_the_same_page() {
        let (mut doc, _hist, page, ids) = setup();
        assert!(plan(&mut doc, page, Command::MoveBlockToPage { id: ids[0], page }).is_none());
    }

    #[test]
    fn folding_hides_nothing_from_the_model_and_undoes() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockType { id: ids[0], kind: BlockKind::Toggle },
        )
        .unwrap();

        let changes = exec(&mut doc, &mut hist, page, Command::ToggleFold { id: ids[0] }).unwrap();
        assert_eq!(changes, vec![Change::BlockFoldedSet { id: ids[0], folded: true }]);
        assert!(doc.block(ids[0]).unwrap().folded);
        // fold is a view flag: the blocks are all still there to be counted
        assert_eq!(doc.page_blocks(page).len(), 3);

        undo(&mut doc, &mut hist, page);
        assert!(!doc.block(ids[0]).unwrap().folded);
        redo(&mut doc, &mut hist, page);
        assert!(doc.block(ids[0]).unwrap().folded);
    }

    #[test]
    fn turning_a_folded_toggle_into_plain_kind_reopens_it() {
        let (mut doc, mut hist, page, ids) = setup();
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockType { id: ids[0], kind: BlockKind::Toggle },
        )
        .unwrap();
        exec(&mut doc, &mut hist, page, Command::ToggleFold { id: ids[0] }).unwrap();

        // a Paragraph draws no chevron, so it must not stay collapsed
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[0], kind: BlockKind::Quote })
            .unwrap();
        assert!(!doc.block(ids[0]).unwrap().folded);
        undo(&mut doc, &mut hist, page);
        let b = doc.block(ids[0]).unwrap();
        assert_eq!(b.kind, BlockKind::Toggle);
        assert!(b.folded, "undo restores the fold it found");
    }

    #[test]
    fn split_and_duplicate_never_inherit_the_fold() {
        let (mut doc, mut hist, page, ids) = setup();
        let t = ids[0];
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: t, kind: BlockKind::Toggle })
            .unwrap();
        exec(&mut doc, &mut hist, page, Command::ToggleFold { id: t }).unwrap();

        // "second" splits: the tail has no children, so it starts open
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[1], kind: BlockKind::Toggle })
            .unwrap();
        exec(&mut doc, &mut hist, page, Command::ToggleFold { id: ids[1] }).unwrap();
        exec(&mut doc, &mut hist, page, Command::SplitBlock { id: ids[1], caret: 3 }).unwrap();
        let blocks = doc.page_blocks(page);
        assert!(blocks[1].folded, "the head keeps the section closed");
        assert!(!blocks[2].folded, "the tail is a fresh open block");

        exec(&mut doc, &mut hist, page, Command::DuplicateBlock { id: t }).unwrap();
        let blocks = doc.page_blocks(page);
        let copy = blocks.iter().find(|b| b.id != t && b.text == blocks[0].text).unwrap();
        assert!(!copy.folded, "a flat copy has no subtree to hide");
    }

    fn att(n: u64) -> Attachment {
        Attachment {
            id: AttachmentId(n),
            name: format!("photo-{n}.png"),
            file: format!("{n}.png"),
            thumb: String::new(),
            mime: "image/png".into(),
            bytes: 4096,
            width: 640,
            height: 480,
        }
    }

    #[test]
    fn insert_image_adds_a_picture_block_and_undo_drops_only_the_reference() {
        let (mut doc, mut hist, page, ids) = setup();
        let changes = exec(
            &mut doc,
            &mut hist,
            page,
            Command::InsertImage { id: ids[0], attachment: att(77) },
        )
        .unwrap();
        // the row is written before the block that points at it
        assert!(matches!(
            &changes[0],
            Change::AttachmentAdded(a) if a.id == AttachmentId(77)
        ));
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.len(), 4);
        assert!(blocks[1].order > blocks[0].order, "the picture lands after the anchor");
        assert_eq!(blocks[1].kind, BlockKind::Image);
        assert_eq!(blocks[1].attachment, Some(AttachmentId(77)));
        // the file name is the block text: export and Ctrl+F only see text
        assert_eq!(blocks[1].text, "photo-77.png");
        assert_eq!(blocks[1].img_percent, 100);
        let pic = blocks[1].id;

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 3);
        assert!(doc.block(pic).is_none());
        // redo has to find the attachment row still there, without disk
        redo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(pic).unwrap().attachment, Some(AttachmentId(77)));
    }

    #[test]
    fn turning_a_block_into_an_image_replaces_its_text_and_undo_restores_it() {
        let (mut doc, mut hist, page, ids) = setup();
        let id = ids[1];
        exec(&mut doc, &mut hist, page, Command::SetBlockImage { id, attachment: att(5) })
            .unwrap();
        let b = doc.block(id).unwrap();
        assert_eq!(b.kind, BlockKind::Image);
        assert_eq!(b.text, "photo-5.png");
        assert_eq!(b.attachment, Some(AttachmentId(5)));

        undo(&mut doc, &mut hist, page);
        let b = doc.block(id).unwrap();
        assert_eq!(b.kind, BlockKind::Paragraph, "the old kind comes back");
        assert_eq!(b.text, "second", "the words it carried come back too");
        assert_eq!(b.attachment, None);
    }

    #[test]
    fn turning_a_folded_toggle_into_an_image_reopens_it() {
        let (mut doc, mut hist, page, ids) = setup();
        let id = ids[0];
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id, kind: BlockKind::Toggle })
            .unwrap();
        exec(&mut doc, &mut hist, page, Command::ToggleFold { id }).unwrap();

        exec(&mut doc, &mut hist, page, Command::SetBlockImage { id, attachment: att(9) })
            .unwrap();
        assert!(!doc.block(id).unwrap().folded, "a picture draws no chevron");
        undo(&mut doc, &mut hist, page);
        let b = doc.block(id).unwrap();
        assert!(b.folded, "undo restores the fold it found");
        assert_eq!(b.kind, BlockKind::Toggle);
    }

    #[test]
    fn image_width_is_undoable_and_the_same_percent_plans_nothing() {
        let (mut doc, mut hist, page, ids) = setup();
        let id = ids[0];
        exec(&mut doc, &mut hist, page, Command::SetBlockImage { id, attachment: att(3) })
            .unwrap();

        let changes =
            exec(&mut doc, &mut hist, page, Command::SetImageWidth { id, percent: 50 }).unwrap();
        assert_eq!(changes, vec![Change::BlockImageWidthSet { id, percent: 50 }]);
        assert_eq!(doc.block(id).unwrap().img_percent, 50);
        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(id).unwrap().img_percent, 100);

        // back at the default there is nothing left to record
        assert!(plan(&mut doc, page, Command::SetImageWidth { id, percent: 100 }).is_none());
    }

    #[test]
    fn a_language_is_undoable_and_the_language_it_already_has_plans_nothing() {
        let (mut doc, mut hist, page, ids) = setup();
        let id = ids[0];

        let changes =
            exec(&mut doc, &mut hist, page, Command::SetCodeLang { id, lang: Lang::Rust }).unwrap();
        assert_eq!(changes, vec![Change::BlockLangSet { id, lang: Lang::Rust }]);
        assert_eq!(doc.block(id).unwrap().lang, Lang::Rust);
        undo(&mut doc, &mut hist, page);
        assert_eq!(
            doc.block(id).unwrap().lang,
            Lang::Plain,
            "undo puts back the uncoloured block it found"
        );

        // a pick that changes nothing is not a step in the history either
        assert!(plan(&mut doc, page, Command::SetCodeLang { id, lang: Lang::Plain }).is_none());
    }

    /// A non-picture attachment: the stored file is named apart from the
    /// display name, which is what makes this shape different from `att`.
    fn file_att(n: u64) -> Attachment {
        Attachment {
            id: AttachmentId(n),
            name: format!("report-{n}"),
            file: format!("{n}.pdf"),
            thumb: String::new(),
            mime: "application/pdf".into(),
            bytes: 1_842_000,
            width: 0,
            height: 0,
        }
    }

    #[test]
    fn a_file_block_names_the_bytes_and_undo_keeps_them() {
        let (mut doc, mut hist, page, ids) = setup();
        let changes = exec(
            &mut doc,
            &mut hist,
            page,
            Command::InsertFile { id: ids[0], attachment: file_att(41) },
        )
        .unwrap();
        assert!(matches!(
            &changes[0],
            Change::AttachmentAdded(a) if a.id == AttachmentId(41)
        ));
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[1].kind, BlockKind::File);
        assert_eq!(blocks[1].attachment, Some(AttachmentId(41)));
        // the display name, not the `<id>.<ext>` the bytes live under
        assert_eq!(blocks[1].text, "report-41");
        // a row with no width to trade against keeps the default tier
        assert_eq!(blocks[1].img_percent, 100);

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 3, "only the reference went away");
        redo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page)[1].attachment, Some(AttachmentId(41)));
    }

    #[test]
    fn turning_a_block_into_a_file_replaces_its_words_and_undo_restores_them() {
        let (mut doc, mut hist, page, ids) = setup();
        let id = ids[1];
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockFile { id, attachment: file_att(7) },
        )
        .unwrap();
        let b = doc.block(id).unwrap();
        assert_eq!(b.kind, BlockKind::File);
        assert_eq!(b.text, "report-7");
        assert_eq!(b.attachment, Some(AttachmentId(7)));

        undo(&mut doc, &mut hist, page);
        let b = doc.block(id).unwrap();
        assert_eq!(b.kind, BlockKind::Paragraph);
        assert_eq!(b.text, "second");
        assert_eq!(b.attachment, None);
    }

    #[test]
    fn a_picture_and_a_file_can_point_at_the_same_attachment_row() {
        // The kinds differ only in how they paint one set of bytes, so nothing
        // is owned between them — the case a copy of a picture into a file
        // row actually produces.
        let (mut doc, mut hist, page, ids) = setup();
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockImage { id: ids[0], attachment: att(3) },
        )
        .unwrap();
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockFile { id: ids[1], attachment: att(3) },
        )
        .unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().attachment, Some(AttachmentId(3)));
        assert_eq!(doc.block(ids[1]).unwrap().attachment, Some(AttachmentId(3)));

        // undoing the file row must not take the picture's pointer with it
        undo(&mut doc, &mut hist, page);
        let picture = doc.block(ids[0]).unwrap();
        assert_eq!(picture.attachment, Some(AttachmentId(3)));
        let back = doc.block(ids[1]).unwrap();
        assert_eq!(back.kind, BlockKind::Paragraph);
        assert_eq!(back.attachment, None);
    }

    /// The empty page had no front door: every insert is anchored to a block,
    /// and a page the user just created has none.
    #[test]
    fn an_empty_page_takes_one_paragraph_and_undo_empties_it_again() {
        let mut doc = Document::new(1000);
        let mut hist = History::default();
        let page = PageId(1);
        assert!(doc.page_blocks(page).is_empty());
        let changes = exec(
            &mut doc,
            &mut hist,
            page,
            Command::AppendBlock {
                kind: BlockKind::Paragraph,
                text: String::new(),
            },
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(blocks[0].parent, None);
        undo(&mut doc, &mut hist, page);
        assert!(doc.page_blocks(page).is_empty(), "undo gives the empty page back");
        redo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 1);
    }

    #[test]
    fn appending_lands_below_a_containers_whole_subtree() {
        let (mut doc, mut hist, page, ids) = setup();
        to_table(&mut doc, &mut hist, page, ids[0]);
        exec(
            &mut doc,
            &mut hist,
            page,
            Command::AppendBlock {
                kind: BlockKind::Paragraph,
                text: "tail".into(),
            },
        )
        .unwrap();
        let blocks = doc.page_blocks(page);
        let tail = blocks.last().unwrap();
        assert_eq!(tail.text, "tail");
        assert_eq!(tail.parent, None, "a page-level block, not a cell");
        // the grid is still one contiguous run behind its own row, which is
        // what every index reader in the projection assumes
        let table = blocks.iter().find(|b| b.kind == BlockKind::Table).unwrap();
        let run: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.parent == Some(table.id))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(run.len(), 6);
        assert_eq!(blocks[run[0] - 1].id, table.id);
        assert!(run.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn a_grid_or_a_layout_is_not_a_bare_append() {
        let (mut doc, mut hist, page, _) = setup();
        for kind in [BlockKind::Table, BlockKind::Columns, BlockKind::TableCell] {
            assert!(
                exec(
                    &mut doc,
                    &mut hist,
                    page,
                    Command::AppendBlock { kind, text: "".into() }
                )
                .is_none(),
                "{kind:?} must be built by a command that knows its shape"
            );
        }
    }

    // ---- table grid (SPEC §三十七 批次 B) ----

    fn to_table(doc: &mut Document, hist: &mut History, page: PageId, id: BlockId) {
        exec(doc, hist, page, Command::SetBlockType { id, kind: BlockKind::Table }).unwrap();
    }

    fn cells(doc: &Document, page: PageId, table: BlockId) -> Vec<Block> {
        doc.page_blocks(page).iter().filter(|b| b.parent == Some(table)).cloned().collect()
    }

    /// A 3x2 grid whose cells read A0..A2 / B0..B2, from the "first" paragraph.
    fn labeled_table(doc: &mut Document, hist: &mut History, page: PageId, id: BlockId) {
        to_table(doc, hist, page, id);
        let labels = ["A0", "A1", "A2", "B0", "B1", "B2"];
        for (cell, label) in cells(doc, page, id).iter().zip(labels) {
            exec(doc, hist, page, Command::ReplaceText { id: cell.id, text: label.into() })
                .unwrap();
        }
    }

    fn texts(doc: &Document, page: PageId, table: BlockId) -> Vec<String> {
        cells(doc, page, table).iter().map(|c| c.text.clone()).collect()
    }

    #[test]
    fn a_line_becomes_a_grid_that_holds_its_words() {
        let (mut doc, mut hist, page, ids) = setup();
        to_table(&mut doc, &mut hist, page, ids[0]);
        let table = doc.block(ids[0]).unwrap();
        assert_eq!(table.kind, BlockKind::Table);
        assert_eq!(table.columns, TABLE_DEFAULT_COLUMNS);
        // a grid draws no text of its own, so the line's words moved to the
        // top-left cell rather than disappearing with the paragraph
        assert_eq!(table.text, "");
        let grid = cells(&doc, page, ids[0]);
        assert_eq!(grid.len(), (TABLE_DEFAULT_COLUMNS * TABLE_DEFAULT_ROWS) as usize);
        assert!(grid.iter().all(|b| b.kind == BlockKind::TableCell && b.parent == Some(ids[0])));
        assert_eq!(grid[0].text, "first");
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks[0].id, ids[0]);
        assert_eq!(blocks[1].id, grid[0].id);
        assert_eq!(blocks.last().unwrap().id, ids[2]);

        undo(&mut doc, &mut hist, page);
        let back = doc.block(ids[0]).unwrap();
        assert_eq!((back.kind, back.text.as_str(), back.columns), (BlockKind::Paragraph, "first", 0));
        assert_eq!(doc.page_blocks(page).len(), 3);
        redo(&mut doc, &mut hist, page);
        assert_eq!(cells(&doc, page, ids[0]).len(), 6);
        assert_eq!(doc.block(ids[0]).unwrap().columns, TABLE_DEFAULT_COLUMNS);
    }

    #[test]
    fn turning_a_block_that_owns_children_into_a_table_is_refused() {
        let (mut doc, mut hist, page, ids) = setup();
        let order = OrderKey::between(Some(doc.block(ids[1]).unwrap().order), None).unwrap();
        doc.apply(&[Change::BlockMoved { id: ids[2], parent: Some(ids[1]), order }]);
        // ids[1] already has a child: as a table it would hide behind the grid
        assert!(exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockType { id: ids[1], kind: BlockKind::Table }
        )
        .is_none());
        assert_eq!(doc.block(ids[1]).unwrap().kind, BlockKind::Paragraph);
        // a block with nothing under it still converts
        assert!(exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockType { id: ids[0], kind: BlockKind::Table }
        )
        .is_some());
    }

    #[test]
    fn adding_a_row_appends_whole_rows_and_undo_drops_them() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);

        exec(&mut doc, &mut hist, page, Command::TableAddRow { id: ids[0], row: 2 }).unwrap();
        assert_eq!(
            texts(&doc, page, ids[0]),
            ["A0", "A1", "A2", "B0", "B1", "B2", "", "", ""]
        );
        // a row can also land at the top — the slot is where the cells sort
        exec(&mut doc, &mut hist, page, Command::TableAddRow { id: ids[0], row: 0 }).unwrap();
        assert_eq!(texts(&doc, page, ids[0])[..3], ["", "", ""]);
        assert_eq!(cells(&doc, page, ids[0]).len(), 12);
        // one past the last row is the append the Tab key asks for; two past
        // is a row that cannot exist, so the plan refuses it
        exec(&mut doc, &mut hist, page, Command::TableAddRow { id: ids[0], row: 4 }).unwrap();
        assert_eq!(cells(&doc, page, ids[0]).len(), 15);
        assert!(exec(&mut doc, &mut hist, page, Command::TableAddRow { id: ids[0], row: 6 }).is_none());
        // and a row is the unit: no table command runs on a non-table
        assert!(exec(&mut doc, &mut hist, page, Command::TableAddRow { id: ids[1], row: 0 }).is_none());

        for _ in 0..3 {
            undo(&mut doc, &mut hist, page);
        }
        assert_eq!(texts(&doc, page, ids[0]), ["A0", "A1", "A2", "B0", "B1", "B2"]);
    }

    #[test]
    fn adding_a_column_puts_one_cell_in_every_row() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);

        exec(&mut doc, &mut hist, page, Command::TableAddColumn { id: ids[0], col: 1 }).unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().columns, 4);
        // row-major: the new cell of each row sits after that row's column 0
        assert_eq!(
            texts(&doc, page, ids[0]),
            ["A0", "", "A1", "A2", "B0", "", "B1", "B2"]
        );

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(ids[0]).unwrap().columns, 3);
        assert_eq!(texts(&doc, page, ids[0]), ["A0", "A1", "A2", "B0", "B1", "B2"]);
        redo(&mut doc, &mut hist, page);
        assert_eq!(cells(&doc, page, ids[0]).len(), 8);

        // at the far edge, and past the last column
        exec(&mut doc, &mut hist, page, Command::TableAddColumn { id: ids[0], col: 4 }).unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().columns, 5);
        assert_eq!(cells(&doc, page, ids[0]).len(), 10);
        assert!(exec(&mut doc, &mut hist, page, Command::TableAddColumn { id: ids[0], col: 6 }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::TableAddColumn { id: ids[1], col: 0 }).is_none());
    }

    #[test]
    fn deleting_a_row_or_column_takes_only_its_own_cells() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);

        exec(&mut doc, &mut hist, page, Command::TableDeleteRow { id: ids[0], row: 0 }).unwrap();
        assert_eq!(texts(&doc, page, ids[0]), ["B0", "B1", "B2"]);
        undo(&mut doc, &mut hist, page);
        assert_eq!(cells(&doc, page, ids[0]).len(), 6);
        assert_eq!(texts(&doc, page, ids[0]), ["A0", "A1", "A2", "B0", "B1", "B2"]);

        exec(&mut doc, &mut hist, page, Command::TableDeleteColumn { id: ids[0], col: 1 }).unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().columns, 2);
        assert_eq!(texts(&doc, page, ids[0]), ["A0", "A2", "B0", "B2"]);
        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(ids[0]).unwrap().columns, 3);
        assert_eq!(texts(&doc, page, ids[0]), ["A0", "A1", "A2", "B0", "B1", "B2"]);

        // out of range, and the last row / last column are protected
        assert!(exec(&mut doc, &mut hist, page, Command::TableDeleteRow { id: ids[0], row: 2 }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::TableDeleteColumn { id: ids[0], col: 3 }).is_none());
        exec(&mut doc, &mut hist, page, Command::TableDeleteRow { id: ids[0], row: 1 }).unwrap();
        assert_eq!(cells(&doc, page, ids[0]).len(), 3);
        assert!(exec(&mut doc, &mut hist, page, Command::TableDeleteRow { id: ids[0], row: 0 }).is_none());
        exec(&mut doc, &mut hist, page, Command::TableDeleteColumn { id: ids[0], col: 2 }).unwrap();
        exec(&mut doc, &mut hist, page, Command::TableDeleteColumn { id: ids[0], col: 1 }).unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().columns, 1);
        assert_eq!(texts(&doc, page, ids[0]), ["A0"]);
        assert!(exec(&mut doc, &mut hist, page, Command::TableDeleteColumn { id: ids[0], col: 0 }).is_none());
        // the table itself never dies from the grid toolbar
        assert_eq!(cells(&doc, page, ids[0]).len(), 1);
        assert_eq!(doc.block(ids[0]).unwrap().kind, BlockKind::Table);
    }

    #[test]
    fn flattening_a_grid_gives_the_words_back_as_paragraphs() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);

        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockType { id: ids[0], kind: BlockKind::Heading2 },
        )
        .unwrap();
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks[0].kind, BlockKind::Heading2);
        assert_eq!(blocks[0].columns, 0);
        // the cells are visible blocks again, in grid order, words and all
        assert_eq!(
            texts(&doc, page, ids[0]),
            ["A0", "A1", "A2", "B0", "B1", "B2"]
        );
        assert!(cells(&doc, page, ids[0]).iter().all(|b| b.kind == BlockKind::Paragraph));

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(ids[0]).unwrap().kind, BlockKind::Table);
        assert_eq!(doc.block(ids[0]).unwrap().columns, 3);
        assert!(cells(&doc, page, ids[0]).iter().all(|b| b.kind == BlockKind::TableCell));
    }

    #[test]
    fn a_cell_is_not_a_prose_block() {
        let (mut doc, mut hist, page, ids) = setup();
        to_table(&mut doc, &mut hist, page, ids[0]);
        let grid = ids_from(&cells(&doc, page, ids[0]));

        // it moves with its grid, not with Alt+Up/Down
        let before = doc.page_blocks(page).iter().map(|b| b.id).collect::<Vec<_>>();
        for delta in [1i32, -1] {
            assert!(exec(&mut doc, &mut hist, page, Command::MoveBlock { id: grid[1], delta }).is_none());
        }
        assert_eq!(doc.page_blocks(page).iter().map(|b| b.id).collect::<Vec<_>>(), before);
        // and it dies with a row or a column, never alone or merged away
        assert!(exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: grid[1] }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::MergeBackward { id: grid[1] }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::MergeBackward { id: ids[0] }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SplitBlock { id: grid[1], caret: 0 }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SplitBlock { id: ids[0], caret: 0 }).is_none());
        // no bare kind builds a grid, and the Turn-into menu keeps out of it
        assert!(exec(&mut doc, &mut hist, page, Command::InsertBlockAfter { id: ids[1], kind: BlockKind::Table, text: "".into() }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::InsertBlockAfter { id: ids[1], kind: BlockKind::TableCell, text: "".into() }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SetBlockType { id: grid[1], kind: BlockKind::Paragraph }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[1], kind: BlockKind::TableCell }).is_none());
        assert_eq!(doc.page_blocks(page).len(), before.len());
    }

    fn ids_from(blocks: &[Block]) -> Vec<BlockId> {
        blocks.iter().map(|b| b.id).collect()
    }

    #[test]
    fn a_table_copy_carries_its_own_grid() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);

        exec(&mut doc, &mut hist, page, Command::DuplicateBlock { id: ids[0] }).unwrap();
        let original = ids[0];
        let copy = doc
            .page_blocks(page)
            .iter()
            .find(|b| b.kind == BlockKind::Table && b.id != original)
            .unwrap()
            .id;
        assert_eq!(doc.block(copy).unwrap().columns, 3);
        // the copy is ordered as its own grid: two rows of three, same words
        assert_eq!(texts(&doc, page, copy), ["A0", "A1", "A2", "B0", "B1", "B2"]);
        assert_eq!(cells(&doc, page, original).len(), 6);
        // and it landed after the source's whole run, so both read as tables
        let order = doc.page_blocks(page).iter().map(|b| b.parent).collect::<Vec<_>>();
        assert_eq!(order, vec![
            None,
            Some(original),
            Some(original),
            Some(original),
            Some(original),
            Some(original),
            Some(original),
            None,
            Some(copy),
            Some(copy),
            Some(copy),
            Some(copy),
            Some(copy),
            Some(copy),
            None,
            None,
        ]);

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(copy), None);
        assert_eq!(cells(&doc, page, original).len(), 6);
    }

    #[test]
    fn a_ragged_grid_is_not_editable() {
        // grid() refuses a cell count that is not a whole number of rows rather
        // than guess which row is short — so does every command built on it.
        let (mut doc, mut hist, page, ids) = setup();
        to_table(&mut doc, &mut hist, page, ids[0]);
        let stray = cells(&doc, page, ids[0])[4].id;
        doc.apply(&[Change::BlockDeleted { id: stray }]);
        assert_eq!(cells(&doc, page, ids[0]).len(), 5);
        for cmd in [
            Command::TableAddRow { id: ids[0], row: 2 },
            Command::TableAddColumn { id: ids[0], col: 0 },
            Command::TableDeleteRow { id: ids[0], row: 0 },
            Command::TableDeleteColumn { id: ids[0], col: 0 },
            Command::DuplicateBlock { id: ids[0] },
        ] {
            assert!(exec(&mut doc, &mut hist, page, cmd.clone()).is_none(), "{cmd:?} ran on a ragged grid");
        }
    }

    #[test]
    fn cells_key_into_one_gap_as_a_batch() {
        // Each insert halves a gap, so a batch keyed one at a time would run
        // out before the row finished. keys_in_gaps plans the whole batch, and
        // renumbers once when the gap cannot hold it.
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);
        for _ in 0..40 {
            let row = cells(&doc, page, ids[0]).len() / 3;
            exec(&mut doc, &mut hist, page, Command::TableAddRow { id: ids[0], row }).unwrap();
        }
        assert_eq!(cells(&doc, page, ids[0]).len(), 6 + 40 * 3);
        // still a grid: whole rows, and every cell sorts inside its table
        let grid = cells(&doc, page, ids[0]);
        let blocks = doc.page_blocks(page);
        let tslot = blocks.iter().position(|b| b.id == ids[0]).unwrap();
        assert_eq!(blocks[tslot + 1].id, grid[0].id);
        assert_eq!(blocks[tslot + grid.len()].id, grid[grid.len() - 1].id);
        assert_eq!(blocks[tslot + grid.len() + 1].id, ids[1]);
    }

    #[test]
    fn undoing_a_table_delete_brings_the_grid_back() {
        // the delete cascades, in memory and in storage, so the revert list has
        // to carry the cells too — a table restored without its grid reads as
        // an empty one and the words are gone for good
        let (mut doc, mut hist, page, ids) = setup();
        labeled_table(&mut doc, &mut hist, page, ids[0]);
        assert_eq!(cells(&doc, page, ids[0]).len(), 6);
        exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: ids[0] }).unwrap();
        assert_eq!(doc.page_blocks(page).len(), 2);
        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(ids[0]).unwrap().kind, BlockKind::Table);
        assert_eq!(texts(&doc, page, ids[0]), ["A0", "A1", "A2", "B0", "B1", "B2"]);
        // and the grid is still in its own slot run, so the row projection
        // reads three columns twice rather than a table that ends mid-row
        let order: Vec<Option<BlockId>> =
            doc.page_blocks(page).iter().map(|b| b.parent).collect();
        assert_eq!(
            order,
            vec![
                None,
                Some(ids[0]),
                Some(ids[0]),
                Some(ids[0]),
                Some(ids[0]),
                Some(ids[0]),
                Some(ids[0]),
                None,
                None
            ]
        );
        // redo takes the whole thing away again
        crate::core::command::redo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 2);
    }

    #[test]
    fn undoing_a_toggle_delete_brings_its_children_back() {
        let (mut doc, mut hist, page, ids) = setup();
        for id in [&ids[0], &ids[1]] {
            exec(&mut doc, &mut hist, page, Command::SetBlockType { id: *id, kind: BlockKind::Bullet })
                .unwrap();
        }
        exec(&mut doc, &mut hist, page, Command::IndentList { id: ids[1] }).unwrap();
        exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[0], kind: BlockKind::Toggle })
            .unwrap();
        assert_eq!(doc.block(ids[1]).unwrap().parent, Some(ids[0]));
        exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: ids[0] }).unwrap();
        assert_eq!(doc.page_blocks(page).len(), 1);
        undo(&mut doc, &mut hist, page);
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(), ["first", "second", "third"]);
        assert_eq!(blocks[1].parent, Some(ids[0]));
    }

    #[test]
    fn undoing_a_merge_that_ate_a_subtree_restores_it() {
        // a merged block is deleted, so its children went with it
        let (mut doc, mut hist, page, ids) = setup();
        for id in [&ids[1], &ids[2]] {
            exec(&mut doc, &mut hist, page, Command::SetBlockType { id: *id, kind: BlockKind::Bullet })
                .unwrap();
        }
        exec(&mut doc, &mut hist, page, Command::IndentList { id: ids[2] }).unwrap();
        exec(&mut doc, &mut hist, page, Command::MergeBackward { id: ids[1] }).unwrap();
        assert_eq!(doc.page_blocks(page).len(), 1);
        undo(&mut doc, &mut hist, page);
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(), ["first", "second", "third"]);
        assert_eq!(blocks[2].parent, Some(ids[1]));
    }

    // ---- columns layout (SPEC §三十七 批次 B) ----

    fn to_columns(doc: &mut Document, hist: &mut History, page: PageId, id: BlockId) {
        exec(doc, hist, page, Command::SetBlockType { id, kind: BlockKind::Columns }).unwrap();
    }

    /// A layout's boxes, left to right.
    fn boxes(doc: &Document, page: PageId, layout: BlockId) -> Vec<Block> {
        doc.page_blocks(page)
            .iter()
            .filter(|b| b.parent == Some(layout) && b.kind == BlockKind::Column)
            .cloned()
            .collect()
    }

    /// What one box shows: its direct content, in order.
    fn lines(doc: &Document, page: PageId, column: BlockId) -> Vec<Block> {
        column_children(doc, page, column)
    }

    /// Append a labelled line to a box, as the empty-box click does.
    fn add_line(doc: &mut Document, hist: &mut History, page: PageId, column: BlockId, label: &str) -> BlockId {
        let changes =
            exec(doc, hist, page, Command::ColumnsAddBlock { id: column }).unwrap();
        let id = changes
            .iter()
            .find_map(|c| match c {
                Change::BlockInserted(b) => Some(b.id),
                _ => None,
            })
            .unwrap();
        exec(doc, hist, page, Command::ReplaceText { id, text: label.into() }).unwrap();
        id
    }

    /// A 2-box layout reading A0 / A1 and B0 / B1, from the "first" paragraph.
    fn labeled_layout(doc: &mut Document, hist: &mut History, page: PageId, id: BlockId) {
        to_columns(doc, hist, page, id);
        let bs = boxes(doc, page, id);
        for (i, b) in bs.iter().enumerate() {
            let first = lines(doc, page, b.id)[0].id;
            let label = ["A", "B"][i];
            exec(doc, hist, page, Command::ReplaceText { id: first, text: format!("{label}0") })
                .unwrap();
            add_line(doc, hist, page, b.id, &format!("{label}1"));
        }
    }

    #[test]
    fn a_line_becomes_a_layout_that_holds_its_words() {
        let (mut doc, mut hist, page, ids) = setup();
        to_columns(&mut doc, &mut hist, page, ids[0]);
        let layout = doc.block(ids[0]).unwrap();
        assert_eq!(layout.kind, BlockKind::Columns);
        assert_eq!(layout.columns, COLUMNS_DEFAULT);
        // a layout draws no text of its own, so the line's words moved into the
        // first box rather than disappearing with the paragraph
        assert_eq!(layout.text, "");
        let bs = boxes(&doc, page, ids[0]);
        assert_eq!(bs.len(), 2);
        let left = lines(&doc, page, bs[0].id);
        let right = lines(&doc, page, bs[1].id);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].kind, BlockKind::Paragraph);
        assert_eq!(left[0].text, "first");
        assert_eq!(right[0].text, "");
        // the boxes are the layout's only direct children, and each sorts
        // before its own content
        let parents = doc.page_blocks(page).iter().map(|b| b.parent).collect::<Vec<_>>();
        assert_eq!(parents, vec![
            None,
            Some(ids[0]),
            Some(bs[0].id),
            Some(ids[0]),
            Some(bs[1].id),
            None,
            None,
        ]);

        undo(&mut doc, &mut hist, page);
        let back = doc.block(ids[0]).unwrap();
        assert_eq!(back.kind, BlockKind::Paragraph);
        assert_eq!(back.columns, 0);
        assert_eq!(back.text, "first");
        assert_eq!(doc.page_blocks(page).len(), 3);
    }

    #[test]
    fn adding_a_box_gives_it_a_line_and_deleting_one_reflows_its_words() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);

        exec(&mut doc, &mut hist, page, Command::ColumnsAddColumn { id: ids[0] }).unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().columns, 3);
        let bs = boxes(&doc, page, ids[0]);
        assert_eq!(bs.len(), 3);
        // the new box is not a dead end: it arrives with a line to type into
        assert_eq!(lines(&doc, page, bs[2].id).len(), 1);
        assert_eq!(
            doc.page_blocks(page).iter().filter(|b| b.parent == Some(bs[2].id)).count(),
            1
        );
        // three is as wide as the layout goes
        assert!(exec(&mut doc, &mut hist, page, Command::ColumnsAddColumn { id: ids[0] }).is_none());

        exec(&mut doc, &mut hist, page, Command::ColumnsDeleteColumn { id: ids[0] }).unwrap();
        assert_eq!(doc.block(ids[0]).unwrap().columns, 2);
        // nothing was deleted but the shape: the last box's line moved into
        // the one before it
        assert_eq!(lines(&doc, page, boxes(&doc, page, ids[0])[1].id).len(), 3);
        assert_eq!(boxes(&doc, page, ids[0]).len(), 2);

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(ids[0]).unwrap().columns, 3);
        assert_eq!(boxes(&doc, page, ids[0]).len(), 3);
        assert_eq!(lines(&doc, page, boxes(&doc, page, ids[0])[2].id).len(), 1);

        // and never below two: the layout itself is not the delete target
        exec(&mut doc, &mut hist, page, Command::ColumnsDeleteColumn { id: ids[0] }).unwrap();
        assert!(exec(&mut doc, &mut hist, page, Command::ColumnsDeleteColumn { id: ids[0] }).is_none());
        assert_eq!(doc.block(ids[0]).unwrap().kind, BlockKind::Columns);
    }

    #[test]
    fn an_empty_box_takes_the_click_as_a_request_for_a_line() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);
        let bs = boxes(&doc, page, ids[0]);
        for line in lines(&doc, page, bs[1].id) {
            exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: line.id }).unwrap();
        }
        assert!(lines(&doc, page, bs[1].id).is_empty());

        let new = add_line(&mut doc, &mut hist, page, bs[1].id, "typed");
        // it sorts inside the box: right after the container, before whatever
        // follows it, so the next box still reads as the next box
        let blocks = doc.page_blocks(page);
        let slot = blocks.iter().position(|b| b.id == bs[1].id).unwrap();
        assert_eq!(blocks[slot + 1].id, new);
        assert_eq!(blocks[slot + 1].parent, Some(bs[1].id));
        assert_eq!(doc.block(new).unwrap().kind, BlockKind::Paragraph);

        undo(&mut doc, &mut hist, page);
        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(new), None);
        assert!(lines(&doc, page, bs[1].id).is_empty());

        // only a box has boxes to fill
        assert!(exec(&mut doc, &mut hist, page, Command::ColumnsAddBlock { id: ids[0] }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::ColumnsAddBlock { id: new }).is_none());
    }

    #[test]
    fn flattening_a_layout_keeps_the_words_and_drops_the_shape() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);

        exec(
            &mut doc,
            &mut hist,
            page,
            Command::SetBlockType { id: ids[0], kind: BlockKind::Heading2 },
        )
        .unwrap();
        let blocks = doc.page_blocks(page);
        assert_eq!(blocks[0].kind, BlockKind::Heading2);
        assert_eq!(blocks[0].columns, 0);
        assert!(!blocks.iter().any(|b| b.kind == BlockKind::Column));
        // the boxes are gone; their lines stayed in the same display order and
        // moved up under the heading, which is what a toggle does with them
        assert_eq!(
            blocks.iter().filter(|b| b.parent == Some(ids[0])).map(|b| b.text.as_str()).collect::<Vec<_>>(),
            ["A0", "A1", "B0", "B1"]
        );

        undo(&mut doc, &mut hist, page);
        let back = doc.block(ids[0]).unwrap();
        assert_eq!(back.kind, BlockKind::Columns);
        assert_eq!(back.columns, 2);
        let bs = boxes(&doc, page, ids[0]);
        assert_eq!(bs.len(), 2);
        assert_eq!(texts_of(&lines(&doc, page, bs[0].id)), ["A0", "A1"]);
        assert_eq!(texts_of(&lines(&doc, page, bs[1].id)), ["B0", "B1"]);
    }

    fn texts_of(blocks: &[Block]) -> Vec<String> {
        blocks.iter().map(|b| b.text.clone()).collect()
    }

    #[test]
    fn a_box_is_not_a_prose_block() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);
        let bs = boxes(&doc, page, ids[0]);
        let item = lines(&doc, page, bs[0].id)[0].id;
        let before = doc.page_blocks(page).iter().map(|b| b.id).collect::<Vec<_>>();

        // a box dies with its layout, never alone, and its kind is not a menu
        // choice
        assert!(exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: bs[0].id }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::MergeBackward { id: bs[0].id }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SetBlockType { id: bs[0].id, kind: BlockKind::Paragraph }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SetBlockType { id: ids[1], kind: BlockKind::Column }).is_none());
        // a line stays in its box: it cannot walk out with Alt+Up, be promoted
        // out of it, or host a second container
        assert!(exec(&mut doc, &mut hist, page, Command::MoveBlock { id: item, delta: -1 }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::MoveBlock { id: bs[0].id, delta: 1 }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::OutdentList { id: item }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SetBlockType { id: item, kind: BlockKind::Table }).is_none());
        assert!(exec(&mut doc, &mut hist, page, Command::SetBlockType { id: item, kind: BlockKind::Columns }).is_none());
        // a container does not build inside a container
        assert!(exec(&mut doc, &mut hist, page, Command::InsertBlockAfter { id: bs[0].id, kind: BlockKind::Paragraph, text: "".into() }).is_none());
        assert_eq!(doc.page_blocks(page).iter().map(|b| b.id).collect::<Vec<_>>(), before);
    }

    #[test]
    fn editing_inside_a_box_stays_inside_it() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);
        let bs = boxes(&doc, page, ids[0]);
        let item = lines(&doc, page, bs[0].id)[0].id;

        // Enter splits the line into two lines of the same box
        exec(&mut doc, &mut hist, page, Command::SplitBlock { id: item, caret: 1 }).unwrap();
        let after = lines(&doc, page, bs[0].id);
        assert_eq!(texts_of(&after), ["A", "0", "A1"]);
        assert!(after.iter().all(|b| b.parent == Some(bs[0].id)));
        // and backspace at the start of a box's first line does not eat the
        // box: its neighbour is a different parent
        assert!(exec(&mut doc, &mut hist, page, Command::MergeBackward { id: after[0].id }).is_none());
        // Alt+Up/Down can reorder a box's own lines — siblings, so the swap is
        // allowed — but never past the box's edge
        exec(&mut doc, &mut hist, page, Command::MoveBlock { id: after[0].id, delta: 1 }).unwrap();
        assert_eq!(texts_of(&lines(&doc, page, bs[0].id)), ["0", "A", "A1"]);
        // ... and the box's first line has nowhere further up to go
        assert!(exec(&mut doc, &mut hist, page, Command::MoveBlock { id: after[1].id, delta: -1 })
            .is_none());
        // the second box is untouched
        assert_eq!(texts_of(&lines(&doc, page, bs[1].id)), ["B0", "B1"]);
    }

    #[test]
    fn a_layout_copy_carries_its_own_boxes() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);

        exec(&mut doc, &mut hist, page, Command::DuplicateBlock { id: ids[0] }).unwrap();
        let original = ids[0];
        let copy = doc
            .page_blocks(page)
            .iter()
            .find(|b| b.kind == BlockKind::Columns && b.id != original)
            .unwrap()
            .id;
        assert_eq!(doc.block(copy).unwrap().columns, 2);
        let cb = boxes(&doc, page, copy);
        assert_eq!(cb.len(), 2);
        assert_eq!(texts_of(&lines(&doc, page, cb[0].id)), ["A0", "A1"]);
        assert_eq!(texts_of(&lines(&doc, page, cb[1].id)), ["B0", "B1"]);
        // the copy is its own layout: different boxes, different lines
        assert!(boxes(&doc, page, original).iter().all(|b| !cb.iter().any(|c| c.id == b.id)));
        // and it landed after the source's whole subtree, so both read as
        // layouts rather than one layout with a stray line in the middle
        let blocks = doc.page_blocks(page);
        let slot = blocks.iter().position(|b| b.id == copy).unwrap();
        assert_eq!(blocks[slot - 1].parent, Some(boxes(&doc, page, original)[1].id));
        assert_eq!(
            blocks.iter().filter(|b| b.parent.is_none()).map(|b| b.id).collect::<Vec<_>>(),
            vec![original, copy, ids[1], ids[2]]
        );

        undo(&mut doc, &mut hist, page);
        assert_eq!(doc.block(copy), None);
        assert_eq!(boxes(&doc, page, original).len(), 2);
    }

    #[test]
    fn undoing_a_layout_delete_brings_its_boxes_back() {
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);
        exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: ids[0] }).unwrap();
        assert_eq!(doc.page_blocks(page).len(), 2);
        undo(&mut doc, &mut hist, page);
        let back = doc.block(ids[0]).unwrap();
        assert_eq!(back.kind, BlockKind::Columns);
        assert_eq!(back.columns, 2);
        // the words are not gone for good: the cascade came back with them
        let bs = boxes(&doc, page, ids[0]);
        assert_eq!(texts_of(&lines(&doc, page, bs[0].id)), ["A0", "A1"]);
        assert_eq!(texts_of(&lines(&doc, page, bs[1].id)), ["B0", "B1"]);
        redo(&mut doc, &mut hist, page);
        assert_eq!(doc.page_blocks(page).len(), 2);
    }

    #[test]
    fn a_line_after_a_layout_lands_outside_it() {
        // "+" on the layout's row means "a block after the layout": the new
        // line must not sort between the layout and its first box
        let (mut doc, mut hist, page, ids) = setup();
        labeled_layout(&mut doc, &mut hist, page, ids[0]);
        let changes = exec(
            &mut doc,
            &mut hist,
            page,
            Command::InsertBlockAfter {
                id: ids[0],
                kind: BlockKind::Paragraph,
                text: "after".into(),
            },
        )
        .unwrap();
        let new = changes
            .iter()
            .find_map(|c| match c {
                Change::BlockInserted(b) => Some(b.id),
                _ => None,
            })
            .unwrap();
        let blocks = doc.page_blocks(page);
        let slot = blocks.iter().position(|b| b.id == new).unwrap();
        assert_eq!(blocks[slot].parent, None);
        // the whole layout is above it, the next paragraph below
        assert_eq!(blocks[slot - 1].parent, Some(boxes(&doc, page, ids[0])[1].id));
        assert_eq!(blocks[slot + 1].id, ids[1]);
    }
}
