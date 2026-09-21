// In-memory document: the editor's truth. Blocks live per page in display
// order. Mutations go through `apply(&[Change])` — the exact change lists
// the persistence layer (M3) will receive — so undo and storage share one
// mutation path (ADR-0012).
//
// M4 scope: block changes only. Page/meta `Change`s are accepted and
// ignored here (the M2 mock workspace still owns the page tree); the M3
// merge wires them through.

use std::collections::HashMap;

use super::persistence::Change;
use super::types::{Block, BlockId, OrderKey, PageId};

#[derive(Debug, Default)]
pub struct Document {
    pages: HashMap<PageId, Vec<Block>>,
    next_id: u64,
}

/// A forward/revert change pair for one executed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub apply: Vec<Change>,
    pub revert: Vec<Change>,
}

impl Document {
    pub fn new(next_id: u64) -> Self {
        Document { pages: HashMap::new(), next_id }
    }

    pub fn alloc_block_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Reserve `n` consecutive ids and return the first (the id-range
    /// counterpart of `alloc_block_id`, for bulk copies like page
    /// duplication — M8_FEEDBACK #2: the allocator is the single id
    /// authority, and a caller that mints ids without reserving them
    /// collides with the next allocation).
    pub fn reserve_block_ids(&mut self, n: u64) -> u64 {
        let start = self.next_id;
        self.next_id += n;
        start
    }

    /// Next id that would be allocated (used to hand fresh ids to copies).
    pub fn next_id_value(&self) -> u64 {
        self.next_id
    }

    /// Forget a page entirely (page deleted from the workspace).
    pub fn drop_page(&mut self, page: PageId) {
        self.pages.remove(&page);
    }

    /// Seed a page's blocks (M2 mock content / bench content). Replaces any
    /// previous content and resets the id generator past the highest id seen,
    /// so later allocations never collide with reloaded data.
    pub fn set_page_blocks(&mut self, page: PageId, mut blocks: Vec<Block>) {
        for b in &blocks {
            self.next_id = self.next_id.max(b.id.0 + 1);
        }
        blocks.sort_by_key(|b| b.order);
        self.pages.insert(page, blocks);
    }

    pub fn page_blocks(&self, page: PageId) -> &[Block] {
        self.pages.get(&page).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every block of every page, in no particular order. A page's blocks are
    /// all in memory from load time, so this is the whole library (SPEC §三十七,
    /// ADR-0037): what a reference scan must see, including the pages nobody has
    /// opened this session.
    pub fn all_blocks(&self) -> impl Iterator<Item = &Block> {
        self.pages.values().flat_map(Vec::as_slice)
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.pages.values().flat_map(Vec::as_slice).find(|b| b.id == id)
    }

    /// Index of the block within its page's display order.
    pub fn index_of(&self, page: PageId, id: BlockId) -> Option<usize> {
        self.pages
            .get(&page)?
            .iter()
            .position(|b| b.id == id)
    }

    /// Re-key every block of the page to evenly spaced orders. Used when a
    /// mid-insert finds no gap. Deliberately NOT emitted as `Change`s: the
    /// relative order is unchanged, so persisted keys may drift from memory
    /// keys without breaking anything (storage only preserves order).
    ///
    /// The stride is wide on purpose: a table inserts several cells into one
    /// gap at a time, and each insert halves it, so the post-renumber gap has
    /// to survive more than one insert.
    pub fn renumber_page(&mut self, page: PageId) {
        if let Some(vec) = self.pages.get_mut(&page) {
            for (i, b) in vec.iter_mut().enumerate() {
                b.order = OrderKey((1 << 32) + (i as u64) * OrderKey::STRIDE);
            }
        }
    }

    fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.pages.values_mut().flat_map(Vec::as_mut_slice).find(|b| b.id == id)
    }

    fn page_of(&self, id: BlockId) -> Option<PageId> {
        self.pages
            .iter()
            .find(|(_, blocks)| blocks.iter().any(|b| b.id == id))
            .map(|(p, _)| *p)
    }

    /// Apply change lists (forward or revert) to the in-memory state.
    /// Page/meta changes are accepted and ignored for now — see module doc.
    pub fn apply(&mut self, changes: &[Change]) {
        for change in changes {
            match change {
                Change::BlockInserted(block) => {
                    let vec = self.pages.entry(block.page).or_default();
                    if vec.iter().all(|b| b.id != block.id) {
                        vec.push(block.clone());
                        vec.sort_by_key(|b| b.order);
                    }
                }
                Change::BlockDeleted { id } => self.remove_recursive(*id),
                Change::BlockTextSet { id, text } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.text = text.clone();
                    }
                }
                Change::BlockKindSet { id, kind } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.kind = *kind;
                    }
                }
                Change::BlockCheckedSet { id, checked } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.checked = *checked;
                    }
                }
                Change::BlockFoldedSet { id, folded } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.folded = *folded;
                    }
                }
                Change::BlockMarksSet { id, marks } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.marks = marks.clone();
                    }
                }
                Change::BlockMoved { id, parent, order } => {
                    let page = self.page_of(*id);
                    if let (Some(page), Some(b)) = (page, self.block_mut(*id)) {
                        b.parent = *parent;
                        b.order = *order;
                        if let Some(vec) = self.pages.get_mut(&page) {
                            vec.sort_by_key(|b| b.order);
                        }
                    }
                }
                Change::BlockMovedToPage { id, page, parent, order } => {
                    // re-key while the block still sits in its old vec (vec
                    // membership is what `page_of` reports), snapshot it,
                    // and only then move the snapshot across
                    if let Some(b) = self.block_mut(*id) {
                        b.page = *page;
                        b.parent = *parent;
                        b.order = *order;
                    }
                    let moved = self.block(*id).cloned();
                    let from = self.page_of(*id);
                    if let Some(from) = from {
                        if let Some(vec) = self.pages.get_mut(&from) {
                            vec.retain(|b| b.id != *id);
                        }
                    }
                    let vec = self.pages.entry(*page).or_default();
                    if let Some(b) = moved {
                        vec.push(b);
                    }
                    vec.sort_by_key(|b| b.order);
                }
                Change::BlockColorSet { id, color, background } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.color = *color;
                        b.background = *background;
                    }
                }
                Change::BlockRefSet { id, page } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.page_ref = *page;
                    }
                }
                // SPEC §三十九 / ADR-0060: the entity a `Database` block draws.
                // The document is where the block's kind and its pointer live —
                // the records and the cells behind the pointer are not blocks at
                // all, so every other §三十九 change falls into the `_` arm at
                // the end of this match and only storage acts on it.
                Change::BlockDbRefSet { id, db } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.db_ref = *db;
                    }
                }
                Change::BlockAttachmentSet { id, attachment } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.attachment = *attachment;
                    }
                }
                Change::BlockImageWidthSet { id, percent } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.img_percent = *percent;
                    }
                }
                Change::BlockColumnsSet { id, columns } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.columns = *columns;
                    }
                }
                Change::BlockLangSet { id, lang } => {
                    if let Some(b) = self.block_mut(*id) {
                        b.lang = *lang;
                    }
                }
                _ => {}
            }
        }
    }

    fn remove_recursive(&mut self, id: BlockId) {
        let descendants: Vec<BlockId> = self
            .pages
            .values()
            .flat_map(Vec::as_slice)
            .filter(|b| b.parent == Some(id))
            .map(|b| b.id)
            .collect();
        for child in descendants {
            self.remove_recursive(child);
        }
        for vec in self.pages.values_mut() {
            vec.retain(|b| b.id != id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::BlockKind;
    use super::super::types::Lang;

    fn block(doc: &mut Document, page: PageId, text: &str, order: u64) -> Block {
        Block {
            id: doc.alloc_block_id(),
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
        }
    }

    use super::super::types::ColorKind;

    use super::super::types::OrderKey;

    #[test]
    fn insert_text_delete_roundtrip() {
        let mut doc = Document::new(100);
        let page = PageId(1);
        let a = block(&mut doc, page, "a", 1);
        doc.set_page_blocks(page, vec![a.clone()]);
        doc.apply(&[Change::BlockTextSet { id: a.id, text: "abc".into() }]);
        assert_eq!(doc.block(a.id).unwrap().text, "abc");
        doc.apply(&[Change::BlockDeleted { id: a.id }]);
        assert!(doc.page_blocks(page).is_empty());
    }

    #[test]
    fn delete_cascades_descendants() {
        let mut doc = Document::new(100);
        let page = PageId(1);
        let root = block(&mut doc, page, "root", 1);
        let mut child = block(&mut doc, page, "child", 2);
        child.parent = Some(root.id);
        let mut grand = block(&mut doc, page, "grand", 3);
        grand.parent = Some(child.id);
        doc.set_page_blocks(page, vec![root.clone(), child.clone(), grand.clone()]);
        doc.apply(&[Change::BlockDeleted { id: root.id }]);
        assert!(doc.page_blocks(page).is_empty());
    }

    #[test]
    fn block_inserted_keeps_display_order() {
        let mut doc = Document::new(100);
        let page = PageId(1);
        let a = block(&mut doc, page, "a", 10);
        let c = block(&mut doc, page, "c", 30);
        doc.set_page_blocks(page, vec![a.clone(), c.clone()]);
        let mut b = block(&mut doc, page, "b", 20);
        b.id = doc.alloc_block_id();
        doc.apply(&[Change::BlockInserted(b)]);
        let texts: Vec<&str> = doc.page_blocks(page).iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }
}
