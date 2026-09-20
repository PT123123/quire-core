// Editor commands (SPEC §十四): every semantic edit becomes a Command,
// planned against the document into a forward/revert `Entry`. The revert
// list is what undo applies — never a database re-read.

use super::document::{Document, Entry};
use super::history::History;
use super::persistence::Change;
use super::types::{
    Attachment, Block, BlockId, BlockKind, ColorKind, Mark, MarkKind, OrderKey, PageId,
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
            if first.kind == BlockKind::Divider {
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
            if idx == 0 {
                // first block: only an empty one may vanish
                if this.text.is_empty() {
                    Some(Entry {
                        apply: vec![Change::BlockDeleted { id }],
                        revert: vec![Change::BlockInserted(this)],
                    })
                } else {
                    None
                }
            } else {
                let prev = doc.page_blocks(page).get(idx - 1)?.clone();
                if prev.kind == BlockKind::Divider {
                    return None;
                }
                Some(Entry {
                    apply: vec![
                        Change::BlockTextSet { id: prev.id, text: prev.text.clone() + &this.text },
                        Change::BlockDeleted { id },
                    ],
                    revert: vec![
                        Change::BlockInserted(this),
                        Change::BlockTextSet { id: prev.id, text: prev.text },
                    ],
                })
            }
        }

        Command::DeleteBlock { id } => {
            let idx = doc.index_of(page, id)?;
            let this = doc.page_blocks(page).get(idx)?.clone();
            Some(Entry {
                apply: vec![Change::BlockDeleted { id }],
                revert: vec![Change::BlockInserted(this)],
            })
        }

        Command::InsertBlockAfter { id, kind, text } => {
            let idx = doc.index_of(page, id)?;
            doc.page_blocks(page).get(idx)?;
            let order = key_after(doc, page, idx)?;
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

        Command::DuplicateBlock { id } => {
            let idx = doc.index_of(page, id)?;
            let src = doc.page_blocks(page).get(idx)?.clone();
            let order = key_after(doc, page, idx)?;
            let new_id = doc.alloc_block_id();
            let mut copy = src.clone();
            copy.id = new_id;
            copy.order = order;
            // a duplicate is flat (no subtree): it must not start out hiding
            // children that were never copied
            copy.folded = false;
            Some(Entry {
                apply: vec![Change::BlockInserted(copy)],
                revert: vec![Change::BlockDeleted { id: new_id }],
            })
        }

        Command::SetBlockType { id, kind } => {
            let b = doc.block(id)?;
            let old = b.kind;
            if old == kind {
                return None;
            }
            let mut apply = vec![Change::BlockKindSet { id, kind }];
            let mut revert = vec![Change::BlockKindSet { id, kind: old }];
            // Only a Toggle draws a chevron, so a folded block of another
            // kind would hide its subtree with no way back: turning one into
            // a plain kind re-opens it (and undo re-closes it).
            if b.folded && kind != BlockKind::Toggle {
                apply.push(Change::BlockFoldedSet { id, folded: false });
                revert.push(Change::BlockFoldedSet { id, folded: true });
            }
            Some(Entry { apply, revert })
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
            let target = i32::try_from(idx).ok()? + delta;
            if target < 0 {
                return None;
            }
            let neighbor_idx = usize::try_from(target).ok()?;
            let this = blocks.get(idx)?.clone();
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
}
