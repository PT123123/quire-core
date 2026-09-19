// In-page find (SPEC §二十, M7): the data layer under a Ctrl+F bar.
//
// `SearchService::query_in_page` answers a different question — it ranks a
// whole-workspace match list down to one `Hit` per page, with a snippet
// elided by characters. A find bar needs the opposite: every occurrence of
// the term inside the page that is already open, addressed by block and by
// byte range so the caret can be placed there. So the session is built from
// the blocks the editor already holds, in the display order the editor
// already draws them in, and it does no IO — a keystroke rebuilds it in a
// linear scan of one page.
//
// Matching is exact for anything non-ASCII and case-insensitive for ASCII,
// which is the same rule `search_service::snippet` centres on; hits never
// overlap, so "aa" in "aaa" yields one hit, like every editor's find bar.

use crate::core::types::{Block, BlockId};

/// One occurrence. `start` and `end` are byte offsets into the block's text
/// and land on char boundaries — the convention `Mark` and the caret share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindHit {
    pub block: BlockId,
    pub start: usize,
    pub end: usize,
}

/// The ordered hit list for one term, plus the cursor the bar moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindSession {
    term: String,
    hits: Vec<FindHit>,
    /// `None` until the first step, so a new session's `next` lands on hit
    /// one rather than skipping it.
    cursor: Option<usize>,
}

impl FindSession {
    /// Every occurrence of `term` in `blocks`: first by the order the blocks
    /// are given — pass them in display order — then top-down within a block.
    /// A blank term matches nothing.
    pub fn new(term: &str, blocks: &[Block]) -> Self {
        let mut hits = Vec::new();
        if !term.trim().is_empty() {
            for block in blocks {
                for (start, end) in occurrences(&block.text, term) {
                    hits.push(FindHit {
                        block: block.id,
                        start,
                        end,
                    });
                }
            }
        }
        FindSession {
            term: term.to_string(),
            hits,
            cursor: None,
        }
    }

    /// The term as typed, for the bar's own field.
    pub fn term(&self) -> &str {
        &self.term
    }

    /// All hits, in walk order — what a "highlight every match" pass draws.
    pub fn hits(&self) -> &[FindHit] {
        &self.hits
    }

    pub fn total(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// The hit the cursor sits on, `None` before the first step or when the
    /// term matches nothing.
    pub fn current(&self) -> Option<FindHit> {
        self.cursor.map(|i| self.hits[i])
    }

    /// 1-based position of the current hit, for the "3 / 12" label.
    pub fn position(&self) -> Option<usize> {
        self.cursor.map(|i| i + 1)
    }

    /// Step forward, wrapping at the end of the page.
    // A find cursor is not an iterator: it wraps, steps back, and can be read
    // without moving, so `next` here is the find bar's Enter key.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<FindHit> {
        if self.hits.is_empty() {
            return None;
        }
        let i = self.cursor.map_or(0, |i| (i + 1) % self.hits.len());
        self.cursor = Some(i);
        Some(self.hits[i])
    }

    /// Step backward, wrapping at the start.
    pub fn prev(&mut self) -> Option<FindHit> {
        if self.hits.is_empty() {
            return None;
        }
        let len = self.hits.len();
        let i = self.cursor.map_or(len - 1, |i| (i + len - 1) % len);
        self.cursor = Some(i);
        Some(self.hits[i])
    }
}

/// Byte ranges where `term` occurs in `text`, non-overlapping, left to right.
fn occurrences(text: &str, term: &str) -> Vec<(usize, usize)> {
    let needle: Vec<char> = term.chars().collect();
    if needle.is_empty() || needle.len() > text.chars().count() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut from = 0;
    while from + needle.len() <= hay.len() {
        let found = (0..needle.len()).all(|k| {
            let h = hay[from + k].1;
            h == needle[k] || h.eq_ignore_ascii_case(&needle[k])
        });
        if found {
            let end = hay
                .get(from + needle.len())
                .map_or(text.len(), |&(byte, _)| byte);
            out.push((hay[from].0, end));
            from += needle.len();
        } else {
            from += 1;
        }
    }
    out
}
