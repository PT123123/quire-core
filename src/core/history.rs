// Per-page undo/redo stacks. Each page is its own document (Ctrl+Z on page
// B must not resurrect an edit made on page A). Entries carry both change
// lists so undo/redo never re-reads the database (SPEC §十四).

use std::collections::{BTreeSet, HashMap};

use super::document::Entry;
use super::types::PageId;

const CAP: usize = 100;

#[derive(Default)]
struct Stacks {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
}

#[derive(Default)]
pub struct History {
    stacks: HashMap<PageId, Stacks>,
}

impl History {
    pub fn push(&mut self, page: PageId, entry: Entry) {
        let s = self.stacks.entry(page).or_default();
        s.undo.push(entry);
        if s.undo.len() > CAP {
            s.undo.remove(0);
        }
        s.redo.clear();
    }

    /// Pop the last entry to revert; the caller applies `entry.revert` and
    /// gets `entry.apply` back on redo.
    pub fn undo(&mut self, page: PageId) -> Option<Entry> {
        let s = self.stacks.get_mut(&page)?;
        let entry = s.undo.pop()?;
        s.redo.push(entry.clone());
        Some(entry)
    }

    /// Pop the last undone entry to re-apply; the caller applies
    /// `entry.apply`.
    pub fn redo(&mut self, page: PageId) -> Option<Entry> {
        let s = self.stacks.get_mut(&page)?;
        let entry = s.redo.pop()?;
        s.undo.push(entry.clone());
        Some(entry)
    }

    /// Attachment ids an outstanding step still holds, in either direction
    /// (SPEC §三十七, ADR-0037). A reclaim may not delete a picture these
    /// point at: undo would put the block back and the row would name bytes
    /// that no longer exist. Both stacks count — redo is reachable by one
    /// keystroke exactly like undo is.
    pub fn referenced_attachments(&self) -> BTreeSet<i64> {
        let mut ids = BTreeSet::new();
        for stacks in self.stacks.values() {
            for entry in stacks.undo.iter().chain(stacks.redo.iter()) {
                ids.extend(
                    super::persistence::attachment_ids_in(&entry.apply)
                        .chain(super::persistence::attachment_ids_in(&entry.revert))
                        .map(|a| a.as_u64() as i64),
                );
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::persistence::Change;
    use crate::core::types::{
        Attachment, AttachmentId, Block, BlockId, BlockKind, ColorKind, Lang, OrderKey,
    };
    use std::iter::FromIterator;

    fn added(att: u64) -> Change {
        Change::AttachmentAdded(Attachment {
            id: AttachmentId(att),
            name: String::new(),
            file: format!("{att}.png"),
            thumb: String::new(),
            mime: String::new(),
            bytes: 4,
            width: 1,
            height: 1,
        })
    }

    fn picture(block: u64, att: u64) -> Change {
        Change::BlockInserted(Block {
            id: BlockId(block),
            page: PageId(1),
            parent: None,
            order: OrderKey(0),
            kind: BlockKind::Image,
            text: String::new(),
            checked: false,
            marks: Vec::new(),
            color: ColorKind::Default,
            background: ColorKind::Default,
            page_ref: None,
            folded: false,
            attachment: Some(AttachmentId(att)),
            img_percent: 100,
            columns: 0,
            lang: Lang::Plain,
            db_ref: None,
        })
    }

    fn step(apply: Vec<Change>, revert: Vec<Change>) -> Entry {
        Entry { apply, revert }
    }

    fn ids(h: &History) -> Vec<i64> {
        Vec::from_iter(h.referenced_attachments())
    }

    /// What the reclaim is allowed to see: an id survives on either stack, and
    /// falls out of the promise exactly when its step falls off the cap.
    #[test]
    fn both_stacks_protect_and_the_cap_ends_the_protection() {
        let page = PageId(1);
        let mut h = History::default();
        h.push(page, step(vec![picture(10, 7), added(7)], vec![Change::BlockDeleted { id: BlockId(10) }]));
        assert_eq!(ids(&h), vec![7], "one id named twice is still one id");

        h.undo(page);
        assert_eq!(ids(&h), vec![7], "redo is one keystroke away, so it still counts");
        h.redo(page);
        assert_eq!(ids(&h), vec![7]);

        let mut h = History::default();
        for n in 1..=100u64 {
            h.push(page, step(vec![added(n)], vec![Change::MetaDelete { key: format!("k{n}") }]));
        }
        assert_eq!(ids(&h).first(), Some(&1), "a hundred steps, the oldest still stands");
        assert_eq!(ids(&h).len(), 100);
        h.push(page, step(vec![added(101)], vec![]));
        let after = ids(&h);
        assert_eq!(after.len(), 100, "the cap held");
        assert!(!after.contains(&1), "the step that fell off took its protection with it");
        assert!(after.contains(&101));
    }
}
