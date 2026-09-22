// The comparison half of version history (SPEC §三十八 "与当前版本对比").
//
// A version is a page as it stood at some moment, so comparing means putting
// two block sequences side by side and saying which lines arrived, which went
// away, and which one the user edited in place. This module owns that question
// and nothing else: no files, no Slint, no database — `core` may not know about
// any of them (docs/ARCHITECTURE.md hard rule 1).
//
// Identity is the block id. A snapshot carries the page's own rows, so an
// unchanged line has the same id on both sides and a moved line keeps its id
// too, which is what lets this read as "three lines changed" rather than "the
// page was rewritten". The same fact sets the ceiling: a paragraph that
// survives an edit is one changed row, not a word-level diff — the row shows
// what it used to say and what it says now, and the reader judges the distance.

use crate::core::types::{Block, BlockKind};

/// Which side of the comparison a row came from. `Added` means "here now, not
/// in the version"; `Removed` means "in the version, not here now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMark {
    Added,
    Removed,
}

/// One row of the comparison panel. The kind rides along because a line that
/// turned from a heading into a paragraph is two rows of identical text with
/// different marks, and reading that as "nothing happened" would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub mark: DiffMark,
    pub kind: BlockKind,
    pub text: String,
}

/// A block's content, ignoring where it sits and which page owns it — the two
/// things a comparison of what a page *says* is not about. `folded` is excluded
/// for the same reason it is excluded from the page's own content everywhere
/// else: it is view state (SPEC §三十七), and leaving a section collapsed is
/// not an edit to what it says.
fn same_body(a: &Block, b: &Block) -> bool {
    a.kind == b.kind
        && a.text == b.text
        && a.checked == b.checked
        && a.marks == b.marks
        && a.color == b.color
        && a.background == b.background
        && a.page_ref == b.page_ref
        && a.attachment == b.attachment
        && a.img_percent == b.img_percent
        && a.columns == b.columns
        && a.lang == b.lang
        && a.parent == b.parent
}

/// Pairs beyond this count in the middle are not aligned but reported as a
/// wholesale rewrite. A panel that takes a second to open is worse than one
/// that shows more rows, and the extra rows are still true: every line in the
/// middle really did move or change. 262 144 cells is a 512 x 512 middle,
/// i.e. a two-page spread of lines edited at once.
const ALIGN_CELLS: usize = 1 << 18;

/// Compare the version (`before`) with what is on screen now (`after`), in
/// reading order. A row present on both sides with different content arrives as
/// a `Removed`/`Added` pair beside each other, which is the shape people
/// already know how to read.
pub fn compare(before: &[Block], after: &[Block]) -> Vec<DiffLine> {
    // Trim what the two sides agree on before aligning anything: editing one
    // line of a long page then costs one comparison, not one per line, because
    // the alignment below is quadratic in what survives the trim.
    let mut head = 0;
    while head < before.len()
        && head < after.len()
        && before[head].id == after[head].id
        && same_body(&before[head], &after[head])
    {
        head += 1;
    }
    let mut tail = 0;
    while tail < before.len() - head
        && tail < after.len() - head
        && before[before.len() - 1 - tail].id == after[after.len() - 1 - tail].id
        && same_body(&before[before.len() - 1 - tail], &after[after.len() - 1 - tail])
    {
        tail += 1;
    }
    let old = &before[head..before.len() - tail];
    let new = &after[head..after.len() - tail];
    let pairs = if old.len().saturating_mul(new.len()) > ALIGN_CELLS {
        Vec::new()
    } else {
        align(old, new)
    };

    let mut lines = Vec::new();
    let (mut o, mut n) = (0usize, 0usize);
    // The sentinel end-pair drains whatever follows the last match; `o <
    // old.len()` is what keeps it from being read as a matched row.
    for (mo, mn) in pairs.into_iter().chain(std::iter::once((old.len(), new.len()))) {
        while o < mo {
            lines.push(line(DiffMark::Removed, &old[o]));
            o += 1;
        }
        while n < mn {
            lines.push(line(DiffMark::Added, &new[n]));
            n += 1;
        }
        if mo < old.len() {
            if !same_body(&old[mo], &new[mn]) {
                lines.push(line(DiffMark::Removed, &old[mo]));
                lines.push(line(DiffMark::Added, &new[mn]));
            }
            o = mo + 1;
            n = mn + 1;
        }
    }
    lines
}

fn line(mark: DiffMark, block: &Block) -> DiffLine {
    DiffLine {
        mark,
        kind: block.kind,
        text: block.text.clone(),
    }
}

/// Longest-common-subsequence alignment over block ids, returning the matched
/// index pairs in increasing order on both sides. Rows it does not name are
/// exactly the ones the caller reports as arrived or gone.
fn align(old: &[Block], new: &[Block]) -> Vec<(usize, usize)> {
    // `lengths[i][j]` = the common subsequence of the last `i` old rows and the
    // last `j` new rows, so the walk below reads forward from (0, 0).
    let mut lengths = vec![vec![0u32; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lengths[i][j] = if old[i].id == new[j].id {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < old.len() && j < new.len() {
        if old[i].id == new[j].id {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// The name a block kind shows by in the panel. Deliberately not
/// `BlockKind::as_str()`, which is the spelling storage uses.
pub fn kind_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Paragraph => "Text",
        BlockKind::Heading1 => "Heading 1",
        BlockKind::Heading2 => "Heading 2",
        BlockKind::Heading3 => "Heading 3",
        BlockKind::Bullet => "Bulleted",
        BlockKind::Numbered => "Numbered",
        BlockKind::Todo => "To-do",
        BlockKind::Toggle => "Toggle",
        BlockKind::Quote => "Quote",
        BlockKind::Callout => "Callout",
        BlockKind::Code => "Code",
        BlockKind::Math => "Math",
        BlockKind::Divider => "Divider",
        BlockKind::Image => "Image",
        BlockKind::File => "File",
        BlockKind::Table => "Table",
        BlockKind::TableCell => "Table cell",
        BlockKind::Columns => "Columns",
        BlockKind::Column => "Column",
        BlockKind::Page => "Page",
        BlockKind::Link => "Link to page",
        BlockKind::Toc => "Table of contents",
        BlockKind::Embed => "Embed",
        // Track 3's two kinds. The panel is about a page's own body, so these
        // read the same names their slash/insert rows carry (SLASH_ITEMS /
        // INSERT_ITEMS in state.rs) rather than a second spelling.
        BlockKind::Database => "Table view",
        BlockKind::Synced => "Synced block",
    }
}

/// How long ago a version was taken, in the coarsest unit that still says
/// something. Ages rather than clock times on purpose: the app keeps no
/// timezone and no calendar in it, so a timestamp would be either UTC (wrong to
/// the reader) or a conversion nothing here can do — while "4 h ago" is the
/// question the panel is actually answering. A list ordered newest-first makes
/// the absolute moment recoverable by counting.
pub fn age_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return "just now".into();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} d ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months} mo ago");
    }
    format!("{} y ago", months / 12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{BlockId, Mark, MarkKind, OrderKey, PageId};

    /// A top-level block of page 1, in the given position. Every test below
    /// states what it varies — text, kind, marks — and leaves the rest at the
    /// default, because `same_body` is the thing under test and it reads a
    /// twelve-field struct.
    fn blk(id: u64, kind: BlockKind, text: &str) -> Block {
        Block {
            id: BlockId(id),
            page: PageId(1),
            parent: None,
            order: OrderKey(id),
            kind,
            text: text.into(),
            checked: false,
            marks: Vec::new(),
            color: crate::core::ColorKind::Default,
            background: crate::core::ColorKind::Default,
            page_ref: None,
            folded: false,
            attachment: None,
            img_percent: 100,
            columns: 0,
            lang: crate::core::Lang::Plain,
            db_ref: None,
            sync_ref: None,
        }
    }

    fn para(id: u64, text: &str) -> Block {
        blk(id, BlockKind::Paragraph, text)
    }

    /// (mark, kind label, text) — the three columns the panel actually shows, so
    /// a test reads like the row it describes.
    fn rows(lines: &[DiffLine]) -> Vec<(&'static str, &'static str, String)> {
        lines
            .iter()
            .map(|l| {
                (
                    match l.mark {
                        DiffMark::Added => "+",
                        DiffMark::Removed => "-",
                    },
                    kind_label(l.kind),
                    l.text.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn a_page_nobody_touched_compares_to_nothing() {
        let page = vec![para(1, "one"), para(2, "two"), para(3, "three")];
        assert_eq!(rows(&compare(&page, &page)), Vec::new());
    }

    #[test]
    fn one_reworded_line_costs_two_rows_not_one_per_line() {
        // The head and tail trims are the whole point of this: a page whose
        // middle line changed must not report the lines around it.
        let before = vec![para(1, "one"), para(2, "two"), para(3, "three")];
        let after = vec![para(1, "one"), para(2, "TWO"), para(3, "three")];
        assert_eq!(
            rows(&compare(&before, &after)),
            vec![("-", "Text", "two".into()), ("+", "Text", "TWO".into())]
        );
    }

    #[test]
    fn a_line_written_since_the_snapshot_arrives() {
        let before = vec![para(1, "one")];
        let after = vec![para(1, "one"), para(2, "new")];
        assert_eq!(rows(&compare(&before, &after)), vec![("+", "Text", "new".into())]);
    }

    #[test]
    fn a_line_the_page_lost_goes() {
        let before = vec![para(1, "one"), para(2, "doomed")];
        let after = vec![para(1, "one")];
        assert_eq!(rows(&compare(&before, &after)), vec![("-", "Text", "doomed".into())]);
    }

    #[test]
    fn a_moved_line_is_one_that_went_and_one_that_arrived() {
        // Identity is the block id, so a move cannot be shown as "the same line
        // elsewhere" — and it should not be: the panel has no position column,
        // and the reader is asking what the page says differently.
        let before = vec![para(1, "one"), para(2, "two"), para(3, "three")];
        let after = vec![para(2, "two"), para(3, "three"), para(1, "one")];
        assert_eq!(
            rows(&compare(&before, &after)),
            vec![("-", "Text", "one".into()), ("+", "Text", "one".into())]
        );
    }

    #[test]
    fn a_line_that_changed_kind_says_so_even_with_the_same_words() {
        let before = vec![blk(1, BlockKind::Paragraph, "Ship it")];
        let after = vec![blk(1, BlockKind::Heading1, "Ship it")];
        assert_eq!(
            rows(&compare(&before, &after)),
            vec![
                ("-", "Text", "Ship it".into()),
                ("+", "Heading 1", "Ship it".into())
            ]
        );
    }

    #[test]
    fn bold_is_an_edit_and_a_fold_is_not() {
        let plain = para(1, "read this");
        let mut marked = para(1, "read this");
        marked.marks = vec![Mark {
            start: 0,
            end: 4,
            kind: MarkKind::Bold,
            url: String::new(),
            date: None,
        }];
        assert_eq!(
            rows(&compare(&[plain][..], &[marked][..])).len(),
            2,
            "a mark changed what the line says"
        );

        let open = para(1, "read this");
        let mut folded = para(1, "read this");
        folded.folded = true;
        assert_eq!(
            rows(&compare(&[open][..], &[folded][..])),
            Vec::new(),
            "leaving a section collapsed is view state, not content (SPEC §三十七)"
        );
    }

    #[test]
    fn a_page_edited_everywhere_stops_being_aligned() {
        // Past ALIGN_CELLS the middle is reported rather than matched. The shape
        // is the assertion: every old row, then every new row — a rewrite, which
        // is true, and not 520 pairs that happen to look like one.
        let n = 520usize;
        let before: Vec<Block> = (0..n).map(|i| para(i as u64 + 1, "was")).collect();
        let after: Vec<Block> = (0..n).map(|i| para(i as u64 + 1000, "now")).collect();
        let lines = compare(&before, &after);
        assert_eq!(lines.len(), n * 2);
        assert!(lines[..n].iter().all(|l| l.mark == DiffMark::Removed));
        assert!(lines[n..].iter().all(|l| l.mark == DiffMark::Added));
        // The same pair of sides just under the ceiling does align: id 3 on both
        // sides matches, so it contributes no row.
        let before_small: Vec<Block> = (0..512).map(|i| para(i as u64 + 1, "was")).collect();
        let mut after_small = before_small.clone();
        after_small[7].text = "now".into();
        assert_eq!(rows(&compare(&before_small, &after_small)).len(), 2);
    }

    #[test]
    fn every_block_kind_has_its_own_word_in_the_panel() {
        let mut seen: Vec<&str> = Vec::new();
        for kind in BlockKind::ALL {
            let label = kind_label(kind);
            assert!(!label.is_empty());
            assert!(
                !seen.contains(&label),
                "{kind:?} and something before it share the label {label:?}"
            );
            seen.push(label);
        }
        assert_eq!(seen.len(), BlockKind::ALL.len());
    }

    #[test]
    fn an_age_reads_in_the_coarsest_unit_that_still_says_something() {
        let day = 86_400;
        assert_eq!(age_text(0), "just now");
        assert_eq!(age_text(59), "just now");
        assert_eq!(age_text(60), "1 min ago");
        assert_eq!(age_text(59 * 60 + 59), "59 min ago");
        assert_eq!(age_text(3_600), "1 h ago");
        assert_eq!(age_text(23 * 3_600 + 59 * 60), "23 h ago");
        assert_eq!(age_text(day), "1 d ago");
        assert_eq!(age_text(29 * day), "29 d ago");
        assert_eq!(age_text(30 * day), "1 mo ago");
        assert_eq!(age_text(365 * day), "1 y ago");
        // A clock that went backwards (a saved file from a machine with a wrong
        // date) is "just now", not a negative number in the panel.
        assert_eq!(age_text(-500), "just now");
    }
}
