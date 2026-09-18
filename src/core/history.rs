// Per-page undo/redo stacks. Each page is its own document (Ctrl+Z on page
// B must not resurrect an edit made on page A). Entries carry both change
// lists so undo/redo never re-reads the database (SPEC §十四).

use std::collections::HashMap;

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
}
