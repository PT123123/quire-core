// Editor commands (SPEC §十四): every semantic edit becomes a Command,
// planned against the document into a forward/revert `Entry`. The revert
// list is what undo applies — never a database re-read.

use super::document::{Document, Entry};
use super::history::History;
use super::persistence::Change;
use super::types::{Block, BlockId, BlockKind, OrderKey, PageId};

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
    /// Empty block of `kind` directly after `id`.
    InsertBlockAfter { id: BlockId, kind: BlockKind },
    SetBlockType { id: BlockId, kind: BlockKind },
    ToggleTodoChecked { id: BlockId },
    /// Move one slot up (-1) / down (+1) among the page's blocks.
    MoveBlock { id: BlockId, delta: i32 },
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

        Command::InsertBlockAfter { id, kind } => {
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
                text: String::new(),
                checked: false,
            };
            Some(Entry {
                apply: vec![Change::BlockInserted(new)],
                revert: vec![Change::BlockDeleted { id: new_id }],
            })
        }

        Command::SetBlockType { id, kind } => {
            let old = doc.block(id)?.kind;
            if old == kind {
                return None;
            }
            Some(Entry {
                apply: vec![Change::BlockKindSet { id, kind }],
                revert: vec![Change::BlockKindSet { id, kind: old }],
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
            // swapping keys keeps both unique and flips the order
            Some(Entry {
                apply: vec![
                    Change::BlockMoved { id: this.id, parent: None, order: neighbor.order },
                    Change::BlockMoved { id: neighbor.id, parent: None, order: this.order },
                ],
                revert: vec![
                    Change::BlockMoved { id: this.id, parent: None, order: this.order },
                    Change::BlockMoved { id: neighbor.id, parent: None, order: neighbor.order },
                ],
            })
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
    fn per_page_histories_are_independent() {
        let (mut doc, mut hist, page, ids) = setup();
        let page2 = PageId(2);
        exec(&mut doc, &mut hist, page, Command::DeleteBlock { id: ids[0] }).unwrap();
        assert!(undo(&mut doc, &mut hist, page2).is_none(), "page2 has no history");
        assert!(undo(&mut doc, &mut hist, page).is_some());
    }
}
