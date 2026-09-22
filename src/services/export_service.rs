// Markdown export (SPEC §二十六): a page's blocks -> CommonMark text.
//
// The block set is M4's and the inline marks are M6's: `Mark` spans are
// written back as `**bold**`, `*italic*`, `` `code` ``, `~~strike~~` and
// `[text](url)`, so a document that came in through `import_service` leaves
// through here unchanged — and one the editor built by toggling marks leaves
// through here with the same text and the same styling.
//
// Two shapes cannot be written in CommonMark at all, because its inline tree
// is strictly nested: marks of *different* kinds that only partially overlap,
// and styling inside a code span. `normalize` resolves the first by splitting
// the spans at the overlap (every piece keeps its kind, no text is duplicated
// or lost) and drops the second, since a code span's content is literal.
//
// Layout, chosen so re-importing yields the same block list:
// - one block per line group, groups separated by a blank line
// - consecutive items of the same list kind stay tight (one Markdown list),
//   switching kind starts a new list and gets a blank line
// - child blocks are indented two spaces per depth (the importer accepts and
//   flattens that indentation)
// - the file ends with exactly one '\n'; an empty page exports to ""
//
// Inline markers in text get escaped; block markers at the start of a line do
// not (ADR-0016), so a paragraph whose text begins with "- " or "# " comes
// back as a list item. The editor turns those sequences into blocks as it
// types, so the shape is rare; the importer's own rule is what keeps the text.

use std::collections::HashMap;

use crate::core::types::{Block, BlockId, BlockKind, Mark, MarkKind, PageId};

/// Render a page's blocks as Markdown, in display order, with no workspace to
/// ask: every reference keeps the characters its own block stores, and a
/// database block writes nothing (see [`export_page_with`]).
pub fn export_page(blocks: &[Block]) -> String {
    export_page_with(blocks, &|_| None, &|_| None)
}

/// One database view already laid out as a table: the header row and the rows,
/// each cell a display string. **Pre-rendered on purpose** (ADR-0065): a
/// database's content is not its blocks — it is records and values in six
/// tables — and `export_page` is handed blocks and nothing else, so the caller
/// that can already read the database renders the rows and this layer only
/// writes them out. It is the same division the attachment sizes and the math
/// glyphs use: a renderer asks for a value and never fetches one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatabaseTable {
    /// The columns, title first — the view's own order (ADR-0065).
    pub header: Vec<String>,
    /// One entry per record, in the view's order and membership (filters and
    /// sorts included, which is why the caller renders rather than this file).
    pub rows: Vec<Vec<String>>,
}

impl DatabaseTable {
    /// A table with nothing in it writes nothing: a database with no visible
    /// columns has no representation as a grid, and a header-only table would
    /// be a `| | |` line that means less than the blank it replaces.
    pub fn is_empty(&self) -> bool {
        self.header.is_empty()
    }
}

/// The same, plus the two things a block cannot answer for itself (SPEC §四十
/// "页面别名" and §三十九's Markdown answer): what its referenced pages are
/// called **now**, and what each database block shows as a table.
///
/// A reference stores an id, so after a rename the block's own characters are
/// stale while everything on screen — the sidebar, the mention chip, the
/// backlink list — already reads the new title. A named reference that stopped
/// following its page would be exactly the defect that storing ids exists to
/// avoid, so the caller that has a workspace hands one in. Callers that do not
/// (a test, the LAN page writer) go through `export_page` above and get the
/// stored text, which is what this layer did before there was a choice.
///
/// Block-level references (a `Page` or `Link to page` block) are untouched:
/// that slice owns the *inline* channel, and changing a closed milestone's
/// export shape is not a decision a slice may take quietly. ADR-0053 says so
/// out loud.
///
/// `database` is ADR-0065's channel, and `None` means two different things that
/// write the same bytes: a caller with no database reader (the clipboard, the
/// LAN page writer), and a `Database` block whose entity is gone. Both write
/// **nothing at all** — there is no marker line for a database (ADR-0065 argues
/// that at length: the table *is* the representation, and a marker naming an id
/// nothing can resolve on import is a dangling promise), and a deleted entity
/// has no rows to write. What the screen says about it ("(deleted database)",
/// ADR-0060) is screen furniture, not content.
pub fn export_page_with(
    blocks: &[Block],
    title_of: &dyn Fn(PageId) -> Option<String>,
    database: &dyn Fn(BlockId) -> Option<DatabaseTable>,
) -> String {
    export_page_full(blocks, title_of, database, &|_| None)
}

/// The same page again, with one more question the export layer cannot answer
/// for itself (SPEC §四十, ADR-0052 §7): **what does a mirror's source say?**
///
/// A mirror owns no words — that is the whole design — so a caller without this
/// channel exports it as a blank line, which loses the sentence rather than the
/// relationship. What no caller gets back is the relationship itself: Markdown
/// is §二十六's content channel and not a fidelity format, and a block id means
/// nothing in another library, so a mirror flattens into the words it was a
/// second view of. That is what ADR-0032 settled for columns, and inventing a
/// marker would only produce something that cannot be resolved on the way in.
pub fn export_page_full(
    blocks: &[Block],
    title_of: &dyn Fn(PageId) -> Option<String>,
    database: &dyn Fn(BlockId) -> Option<DatabaseTable>,
    sync_of: &dyn Fn(BlockId) -> Option<(String, Vec<Mark>)>,
) -> String {
    let mut out = String::new();
    let mut number = 0;
    // A table's cells are children, so the grid has to be assembled before the
    // walk rather than block by block.
    let grids = table_grids(blocks);
    // A columns layout writes no marker of its own: its containers are a shape,
    // not a block with text, and its content exports at the page's own depth in
    // the order the columns read — the same degradation a grid's cells take.
    let walk: Vec<(usize, &Block)> = display_order(blocks)
        .into_iter()
        .filter(|(_, b)| {
            !matches!(
                b.kind,
                BlockKind::TableCell | BlockKind::Columns | BlockKind::Column
            )
        })
        .map(|(depth, b)| (if inside_columns(blocks, b) { 0 } else { depth }, b))
        .collect();
    for (i, (depth, block)) in walk.iter().enumerate() {
        if i > 0 && !same_list_run(walk[i - 1].1.kind, block.kind) {
            out.push('\n');
        }
        let indent = "  ".repeat(*depth);
        let rendered = match block.kind {
            BlockKind::Table => {
                number = 0;
                render_table(
                    block,
                    grids.get(&block.id).map(Vec::as_slice).unwrap_or(&[]),
                    title_of,
                )
            }
            // ADR-0065: the database's own table, rendered by the caller that
            // can read records — this layer never opens one.
            BlockKind::Database => {
                number = 0;
                database(block.id)
                    .map(|table| render_database(&table))
                    .unwrap_or_default()
            }
            _ => render(block, &mut number, title_of, sync_of),
        };
        for line in rendered.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&indent);
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// One pre-rendered database table as a GitHub-flavored table: a header row, the
/// `---` rule under it, then one line per record. The same shape `render_table`
/// writes for the simple grid, because a reader must not have to tell the two
/// apart in the file.
///
/// A cell's `|` and its newlines become text: neither is representable inside a
/// table cell, and dropping them would lose characters while letting them
/// through would end the column or the row. A ragged row (a caller handing back
/// fewer cells than the header) is padded rather than written short — a short
/// row in GFM silently shifts every following column.
fn render_database(table: &DatabaseTable) -> String {
    if table.header.is_empty() {
        return String::new();
    }
    let cols = table.header.len();
    let cell = |text: &str| text.replace('|', "\\|").replace('\n', " ").replace('\r', " ");
    let head: Vec<String> = table.header.iter().map(|c| cell(c)).collect();
    let mut out = format!("| {} |\n", head.join(" | "));
    let dashes = (0..cols).map(|_| "---").collect::<Vec<_>>().join(" | ");
    out.push_str(&format!("| {dashes} |\n"));
    for row in &table.rows {
        let line: Vec<String> = (0..cols)
            .map(|at| row.get(at).map(|c| cell(c)).unwrap_or_default())
            .collect();
        out.push_str(&format!("| {} |\n", line.join(" | ")));
    }
    out.trim_end().to_string()
}

/// True when the block sits inside a columns layout, at any depth.
fn inside_columns(blocks: &[Block], b: &Block) -> bool {
    let mut parent = b.parent;
    while let Some(pid) = parent {
        let Some(p) = blocks.iter().find(|x| x.id == pid) else {
            break;
        };
        if matches!(p.kind, BlockKind::Columns | BlockKind::Column) {
            return true;
        }
        parent = p.parent;
    }
    false
}

/// The cells of every table on the page, in row-major order.
fn table_grids(blocks: &[Block]) -> HashMap<BlockId, Vec<Block>> {
    let mut out: HashMap<BlockId, Vec<Block>> = HashMap::new();
    for (_, b) in display_order(blocks).into_iter() {
        if b.kind != BlockKind::TableCell {
            continue;
        }
        let Some(parent) = b.parent else { continue };
        let Some(table) = blocks.iter().find(|x| x.id == parent && x.kind == BlockKind::Table)
        else {
            continue; // an orphaned cell is not part of any grid
        };
        out.entry(table.id).or_default().push(b.clone());
    }
    out
}

/// A grid as a GitHub-flavored-Markdown table. The first row is the header
/// row — the same convention the block itself starts with, and the only shape
/// a Markdown table has. A table with no cells writes nothing.
fn render_table(
    table: &Block,
    cells: &[Block],
    title_of: &dyn Fn(PageId) -> Option<String>,
) -> String {
    let cols = table.columns as usize;
    if cols == 0 || cells.is_empty() {
        return String::new();
    }
    let cell = |b: Option<&Block>| {
        b.map(|c| {
            let text = render_inline(&c.text, &c.marks, title_of);
            // a pipe would end the column, a newline the row: neither is
            // representable inside a cell, so both become text
            text.replace('|', "\\|").replace('\n', " ")
        })
        .unwrap_or_default()
    };
    let mut out = String::new();
    for (r, row) in cells.chunks(cols).enumerate() {
        let line = (0..cols).map(|c| cell(row.get(c))).collect::<Vec<_>>().join(" | ");
        out.push_str(&format!("| {line} |\n"));
        if r == 0 {
            let dashes = (0..cols).map(|_| "---").collect::<Vec<_>>().join(" | ");
            out.push_str(&format!("| {dashes} |\n"));
        }
    }
    out.trim_end().to_string()
}

fn is_list(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Bullet | BlockKind::Numbered | BlockKind::Todo
    )
}

/// List items of the same kind form one tight Markdown list; switching kind
/// (bullet -> numbered) starts a new list, which needs a blank line.
fn same_list_run(prev: BlockKind, next: BlockKind) -> bool {
    prev == next && is_list(next)
}

/// One block as Markdown source. `number` is the numbered-list run counter,
/// reset whenever a non-numbered block appears.
fn render(
    block: &Block,
    number: &mut usize,
    title_of: &dyn Fn(PageId) -> Option<String>,
    sync_of: &dyn Fn(BlockId) -> Option<(String, Vec<Mark>)>,
) -> String {
    // a code block is source and a divider has no text: markers stay literal
    let text = match block.kind {
        BlockKind::Code | BlockKind::Divider | BlockKind::Math | BlockKind::Embed => {
            block.text.clone()
        }
        // SPEC §四十 / ADR-0052 §7: a mirror's words are **its source's**, and
        // its own `text` is empty by construction. Reading them here is what
        // "flatten" means — the row leaves as the sentence it was showing, with
        // the source's own marks riding along, because those characters are
        // still a mention even when they are standing in another page.
        //
        // `None` is the source being gone, and it writes nothing: an empty line
        // says more than inventing a marker that no importer can resolve.
        BlockKind::Synced => match sync_of(block.id) {
            Some((words, marks)) => render_inline(&words, &marks, title_of),
            None => String::new(),
        },
        _ => render_inline(&block.text, &block.marks, title_of),
    };
    let text = text.as_str();
    match block.kind {
        BlockKind::Paragraph => {
            *number = 0;
            text.to_string()
        }
        // A mirror exports as a paragraph: the words were already
        // resolved out of the source by the first match above, and a
        // paragraph is the plainest shape a sentence takes (ADR-0052).
        BlockKind::Synced => {
            *number = 0;
            text.to_string()
        }
        BlockKind::Heading1 => heading(1, text),
        BlockKind::Heading2 => heading(2, text),
        BlockKind::Heading3 => heading(3, text),
        BlockKind::Quote => {
            *number = 0;
            prefix_lines("> ", text)
        }
        // Markdown has no callout shape; a quote keeps the emphasis and the
        // text. (Notion itself degrades callouts the same way.)
        BlockKind::Callout => {
            *number = 0;
            prefix_lines("> ", text)
        }
        // CommonMark has no fold syntax, so a toggle degrades exactly like a
        // callout does (SPEC §二十六); the subtree rides along indented,
        // which is the caller's depth walk, and import never restores the fold.
        BlockKind::Toggle => {
            *number = 0;
            prefix_lines("> ", text)
        }
        BlockKind::Code => {
            *number = 0;
            // The info string is the language the block is coloured with, so a
            // page that leaves as Markdown comes back with its colour intact.
            // `Plain` writes nothing, which is the bare fence it matches.
            let info = block.lang.as_str();
            format!("```{info}\n{text}\n```")
        }
        // The formula's own delimiters, on their own lines, which is the shape
        // every Markdown math reader accepts. The source stays verbatim: a
        // LaTeX `\alpha` has no CommonMark spelling, and escaping it would
        // change what the formula *is*.
        BlockKind::Math => {
            *number = 0;
            format!("$$\n{text}\n$$")
        }
        BlockKind::Divider => {
            *number = 0;
            "---".to_string()
        }
        // A contents block has no Markdown shape, and writing its *lines* would
        // put derived data in the document — with block ids that mean nothing
        // in whichever library the file lands in. One marker line says what the
        // block is, renders as nothing anywhere, and reads back as itself.
        BlockKind::Toc => {
            *number = 0;
            "<!-- quire:toc -->".to_string()
        }
        // An embed's Markdown shape is the address, alone on its line: that is
        // what a link card *is*, GFM autolinks it, and every other reader at
        // worst shows a url. No wrapper characters, because a wrapper is
        // something to corrupt when the text is not a well-formed address.
        // The scheme is written even when the block's is missing — the same
        // normalization the card's Open button and the link dialog apply, and
        // what lets the line read back as a card instead of a paragraph.
        BlockKind::Embed => {
            *number = 0;
            crate::core::embed::with_scheme(text)
        }
        BlockKind::Bullet => {
            *number = 0;
            prefix_lines("- ", text)
        }
        BlockKind::Todo => {
            *number = 0;
            prefix_lines(if block.checked { "- [x] " } else { "- [ ] " }, text)
        }
        BlockKind::Numbered => {
            *number += 1;
            format!("{}. {text}", *number)
        }
        // Markdown has no page-embed shape; an in-app link keeps the target
        // openable after re-import (the app resolves quire://page links).
        // Page (owned child) and Link (unowned reference) export alike —
        // the ownership difference does not survive Markdown.
        BlockKind::Page | BlockKind::Link => {
            *number = 0;
            match block.page_ref {
                Some(p) => format!("[{text}](quire://page/{})", p.as_u64()),
                None => text.to_string(),
            }
        }
        // The bytes themselves are not Markdown's business: `text` is the file
        // name and the target is the in-app reference, same shape as a Page
        // block's link. Re-import keeps the name as a link, not a picture.
        BlockKind::Image => {
            *number = 0;
            match block.attachment {
                Some(a) => format!("![{text}](quire://attachment/{})", a.as_u64()),
                None => format!("![{text}]()"),
            }
        }
        // A file has no Markdown shape either, and unlike a picture a link is
        // the right degradation: the importer parses `[text](url)` into a
        // paragraph carrying a link mark, so the name survives *and* stays
        // clickable inside the app.
        BlockKind::File => {
            *number = 0;
            match block.attachment {
                Some(a) => format!("[{text}](quire://attachment/{})", a.as_u64()),
                None => text.to_string(),
            }
        }
        // A grid is written by `render_table`, which is the only place that
        // sees a table's cells; on its own a table block has no text, and a
        // cell is part of a grid rather than a block of its own. The columns
        // containers are filtered out by `export_page` for the same reason.
        BlockKind::Table
        | BlockKind::TableCell
        | BlockKind::Columns
        | BlockKind::Column => {
            *number = 0;
            String::new()
        }
        // Unreachable from the page walk, which intercepts a `Database` block
        // before it gets here (ADR-0065: its content is records, and this layer
        // never opens a database). The arm exists so the match stays exhaustive
        // without a wildcard that would swallow the *next* kind — a new block
        // kind must fail to compile until someone decides what it exports.
        BlockKind::Database => {
            *number = 0;
            String::new()
        }
    }
}

fn heading(level: usize, text: &str) -> String {
    let marker = "#".repeat(level);
    if text.is_empty() {
        marker
    } else {
        format!("{marker} {text}")
    }
}

/// A multi-line block keeps its marker on every line so the whole group is
/// recognized as one block on re-import.
fn prefix_lines(marker: &str, text: &str) -> String {
    text.lines()
        .map(|l| format!("{marker}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── inline marks (M6) ───────────────────────────────────────────────
//
// The writer is a small state machine over the mark boundaries: at each
// boundary it closes the spans that end there and opens the ones that start
// there, then emits the text between. That produces the same nesting the
// importer reads back — `**a _b_ c**` in, bold-with-italic inside, identical
// bytes out — without building a tree.

fn render_inline(text: &str, marks: &[Mark], title_of: &dyn Fn(PageId) -> Option<String>) -> String {
    let spans = normalize(text, marks);
    let mut out = String::new();
    if spans.is_empty() {
        escape_text(text, &mut out);
        return out;
    }
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for m in &spans {
        cuts.push(m.start);
        cuts.push(m.end);
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut open: Vec<&Mark> = Vec::new();
    let mut next = 0;
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        while open.last().is_some_and(|m| m.end <= a) {
            if let Some(m) = open.pop() {
                closer_into(&mut out, m, text);
            }
        }
        while next < spans.len() && spans[next].start == a {
            opener_into(&mut out, &spans[next], text);
            open.push(&spans[next]);
            next += 1;
        }
        // A mention writes its target's *current* title rather than the
        // characters the block holds. The atom is one piece (never cut: it is
        // an atom and `normalize` closes whole spans), so the piece whose
        // bounds equal the mark's is the label.
        if let Some(title) = live_mention_label(&open, a, b, title_of) {
            out.push_str(&title);
            continue;
        }
        // inside a code span or a formula the text is verbatim: escaping there
        // would put the backslash in the document
        if open.iter().any(|m| m.kind == MarkKind::Code || m.kind == MarkKind::Math) {
            out.push_str(&text[a..b]);
        } else {
            escape_text(&text[a..b], &mut out);
        }
    }
    while let Some(m) = open.pop() {
        closer_into(&mut out, m, text);
    }
    out
}

/// The current title of the mention that *is* this piece, or `None` when the
/// piece is something else, the page is gone, or its title cannot be written
/// between `@[` and `]` without being re-read as syntax.
///
/// That last case is a real one and not a theoretical one: the importer takes
/// the label's bytes verbatim and honours `\]` only as an escape *while
/// scanning*, so a title holding a bracket or a newline would come back with
/// the wrong text. Those keep the span's own characters — the same characters
/// the editor shows — which is the pre-existing behaviour rather than a new
/// hole, and the reason is written into ADR-0053.
fn live_mention_label(
    open: &[&Mark],
    a: usize,
    b: usize,
    title_of: &dyn Fn(PageId) -> Option<String>,
) -> Option<String> {
    let m = open
        .iter()
        .find(|m| m.kind == MarkKind::Mention && m.start == a && m.end == b)?;
    let title = crate::core::page_of(&m.url).and_then(title_of)?;
    if title.is_empty() || title.contains(['\\', '[', ']', '\n']) {
        return None;
    }
    Some(title)
}

/// The spans as the writer needs them: on char boundaries, contiguous
/// same-kind runs joined, everything nested or disjoint.
fn normalize(text: &str, marks: &[Mark]) -> Vec<Mark> {
    let mut spans: Vec<Mark> = marks
        .iter()
        .filter(|m| {
            m.start < m.end && text.is_char_boundary(m.start) && text.is_char_boundary(m.end)
        })
        .cloned()
        .collect();
    spans.sort_by_key(|m| (m.start, m.end, kind_order(m.kind)));

    let mut joined: Vec<Mark> = Vec::with_capacity(spans.len());
    for m in spans {
        match joined.last_mut() {
            // two runs of the same kind with nothing between them are written
            // as one: `**a****b**` leaves a four-marker run in the middle, and
            // `` `a``b` `` reads back as a single span holding two backticks.
            // The payload has to match too, and it is compared through
            // `stored_payload` rather than `url` — a date's ISO string lives in
            // `date`, so two adjacent dates with different values would
            // otherwise be joined into one span holding both.
            Some(last)
                if last.kind == m.kind
                    && last.stored_payload() == m.stored_payload()
                    && last.end == m.start =>
            {
                last.end = m.end;
            }
            _ => joined.push(m),
        }
    }
    // a code span's or a formula's content is literal, so nothing strictly
    // inside one can be styled
    let code: Vec<(usize, usize)> = joined
        .iter()
        .filter(|m| m.kind == MarkKind::Code || m.kind == MarkKind::Math)
        .map(|m| (m.start, m.end))
        .collect();
    joined.retain(|m| {
        m.kind == MarkKind::Code
            || m.kind == MarkKind::Math
            || !code
                .iter()
                .any(|&(s, e)| s <= m.start && m.end <= e && (s, e) != (m.start, m.end))
    });

    // split every crossing pair at the overlap; two rounds per span is ample
    let mut guard = joined.len() * 2 + 4;
    while let Some((i, j)) = find_crossing(&joined) {
        if guard == 0 {
            break;
        }
        guard -= 1;
        let (head, tail) = (joined[i].clone(), joined[j].clone());
        let mut split = Vec::with_capacity(joined.len() + 2);
        for (idx, m) in joined.iter().enumerate() {
            if idx == i {
                split_pieces(&mut split, m, tail.start);
            } else if idx == j {
                split_pieces(&mut split, m, head.end);
            } else {
                split.push(m.clone());
            }
        }
        joined = split;
    }
    // unreachable unless the guard ran out: a span that still crosses another
    // has no CommonMark spelling, and writing it half-closed would leak
    // markers into the text — so it goes, and the text stays exact
    while let Some((_, j)) = find_crossing(&joined) {
        joined.remove(j);
    }
    // a formula also swallows whatever *shares* its range: `**a**` under a
    // `$…$` over the same two characters has no spelling, and writing both
    // would export `$**a**$` and import the stars into the formula's source
    let math: Vec<(usize, usize)> = joined
        .iter()
        .filter(|m| m.kind == MarkKind::Math)
        .map(|m| (m.start, m.end))
        .collect();
    joined.retain(|m| {
        m.kind == MarkKind::Math || !math.iter().any(|&(s, e)| s <= m.start && m.end <= e)
    });
    // a mention and a date are atoms for the same reason a formula is: their
    // spelling is `@[label](target)` / `@[iso]`, one token with no room for a
    // nested mark inside it. `**@[a](x)**` has no spelling that reads back as
    // both, so the mark inside the mention goes and the mention stays — the
    // same trade the formula block above makes.
    let atoms: Vec<(usize, usize)> = joined
        .iter()
        .filter(|m| matches!(m.kind, MarkKind::Mention | MarkKind::Date))
        .map(|m| (m.start, m.end))
        .collect();
    joined.retain(|m| {
        matches!(m.kind, MarkKind::Mention | MarkKind::Date)
            || !atoms.iter().any(|&(s, e)| s <= m.start && m.end <= e)
    });
    // a formula whose source starts or ends with a space has no `$…$` spelling
    // — the importer needs a non-space on the inside of both delimiters — so
    // the mark goes and the text stays, which is the same deal as above
    joined.retain(|m| {
        if m.kind != MarkKind::Math {
            return true;
        }
        let src = &text[m.start..m.end];
        !src.starts_with(char::is_whitespace) && !src.ends_with(char::is_whitespace)
    });
    // outer span first at each boundary, so nesting matches the importer
    joined.sort_by_key(|m| (m.start, std::cmp::Reverse(m.end), kind_order(m.kind)));
    joined
}

fn kind_order(kind: MarkKind) -> u8 {
    match kind {
        MarkKind::Bold => 0,
        MarkKind::Italic => 1,
        MarkKind::Strike => 2,
        MarkKind::Code => 3,
        MarkKind::Link => 4,
        MarkKind::Math => 5,
        // the two reference kinds sort after everything: they are atoms, so
        // their position only matters when they share a boundary with a
        // styling mark, and outer-first is what the importer expects
        MarkKind::Mention => 6,
        MarkKind::Date => 7,
    }
}

/// `a` starts inside `b` and ends before it: the one shape the inline tree
/// cannot hold. Returns the indices to split, `a` first.
fn find_crossing(spans: &[Mark]) -> Option<(usize, usize)> {
    for i in 0..spans.len() {
        for j in 0..spans.len() {
            let (a, b) = (&spans[i], &spans[j]);
            if a.start < b.start && b.start < a.end && a.end < b.end {
                return Some((i, j));
            }
        }
    }
    None
}

fn split_pieces(out: &mut Vec<Mark>, m: &Mark, at: usize) {
    if at <= m.start || at >= m.end {
        out.push(m.clone());
        return;
    }
    out.push(Mark {
        start: m.start,
        end: at,
        kind: m.kind,
        url: m.url.clone(),
        date: m.date.clone(),
    });
    out.push(Mark {
        start: at,
        end: m.end,
        kind: m.kind,
        url: m.url.clone(),
        date: m.date.clone(),
    });
}

fn opener_into(out: &mut String, m: &Mark, text: &str) {
    match m.kind {
        MarkKind::Bold => out.push_str("**"),
        MarkKind::Italic => out.push('*'),
        MarkKind::Strike => out.push_str("~~"),
        MarkKind::Code => {
            out.push_str(&"`".repeat(code_run(text, m)));
            if code_pads(text, m) {
                out.push(' ');
            }
        }
        MarkKind::Link => out.push('['),
        MarkKind::Math => out.push('$'),
        // Both reference kinds open the same way; what separates them is the
        // close, and the *label* is the span's own text in both cases — the ISO
        // string for a date, the title as it was when the reference was made
        // for a mention.
        MarkKind::Mention | MarkKind::Date => out.push_str("@["),
    }
}

fn closer_into(out: &mut String, m: &Mark, text: &str) {
    match m.kind {
        MarkKind::Bold => out.push_str("**"),
        MarkKind::Italic => out.push('*'),
        MarkKind::Strike => out.push_str("~~"),
        MarkKind::Code => {
            if code_pads(text, m) {
                out.push(' ');
            }
            out.push_str(&"`".repeat(code_run(text, m)));
        }
        MarkKind::Link => out.push_str(&format!("]({})", link_target(&m.url))),
        MarkKind::Math => out.push('$'),
        // `@[Title](quire://page/12)` — the address, not the label, so what the
        // file carries is the *reference*; a reader that has never heard of
        // Quire still sees a link, and re-importing finds the page again.
        MarkKind::Mention => out.push_str(&format!("]({})", link_target(&m.url))),
        // `@[2026-09-22]` — nothing to close but the bracket: the payload is
        // the visible text, which is also why a date cannot go stale.
        MarkKind::Date => out.push(']'),
    }
}

/// A code span needs a fence longer than any run inside it, so content that
/// holds a backtick still closes on the right marker.
fn code_run(text: &str, m: &Mark) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for c in text[m.start..m.end].chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest + 1
}

/// The two contents a code span cannot carry between bare fences: a backtick
/// at either end would merge with the fence, and the reader strips one space
/// from each end of a padded span. Both cases take the space wrapper, which
/// the importer strips back off.
fn code_pads(text: &str, m: &Mark) -> bool {
    let content = &text[m.start..m.end];
    content.starts_with('`')
        || content.ends_with('`')
        || (!content.trim().is_empty() && content.starts_with(' ') && content.ends_with(' '))
}

/// A target with parentheses or spaces needs the `<…>` form, which is the
/// spelling `import_service` also accepts.
fn link_target(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    if url.chars().any(|c| matches!(c, '(' | ')' | ' ' | '\t')) {
        return format!("<{url}>");
    }
    url.to_string()
}

/// Only markers that could still be read as syntax get a backslash, so plain
/// prose exports as plain prose. `_` is escaped just where it could open
/// emphasis, which keeps `snake_case` readable; the importer's closer rules
/// then have no opener left to match, so an escaped run cannot re-form.
fn escape_text(text: &str, out: &mut String) {
    let mut rest = text;
    let mut prev_alnum = false;
    while let Some(c) = rest.chars().next() {
        let tail = &rest[c.len_utf8()..];
        let escape = match c {
            '*' | '`' | '~' | '[' => true,
            '_' => !prev_alnum && !opens_space(tail) && tail.contains('_'),
            '$' => !opens_space(tail) && dollar_pair_ahead(tail),
            '\\' => tail
                .chars()
                .next()
                .is_some_and(|n| n.is_ascii_punctuation()),
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
        prev_alnum = c.is_alphanumeric();
        rest = tail;
    }
}

/// Could a `$` further ahead close this one as a formula? The importer's own
/// rule, so prose that merely *has* two dollars ("costs $5 and $10") exports
/// without backslashes while `a$b$c`, which would come back as math, does not.
fn dollar_pair_ahead(tail: &str) -> bool {
    let mut prev: Option<char> = None;
    for c in tail.chars() {
        if c == '$' && prev.is_some_and(|p| !p.is_whitespace()) {
            return true;
        }
        prev = Some(c);
    }
    false
}

/// An emphasis opener needs something that is not a space right behind it.
fn opens_space(tail: &str) -> bool {
    tail.chars().next().is_none_or(|c| c.is_whitespace())
}

/// Roots sorted by `order`, each followed by its children recursively.
/// A block whose parent is not in the input is treated as a root, so the
/// flat pages the M4 editor produces (and partial inputs) export fully.
/// Blocks unreachable from any root (a parent cycle, which storage rejects
/// but memory state can hold) are appended at top level — text is never
/// dropped silently.
fn display_order(blocks: &[Block]) -> Vec<(usize, &Block)> {
    let ids: HashMap<BlockId, ()> = blocks.iter().map(|b| (b.id, ())).collect();
    let mut children: HashMap<BlockId, Vec<&Block>> = HashMap::new();
    let mut roots: Vec<&Block> = Vec::new();
    for b in blocks {
        match b.parent.filter(|p| ids.contains_key(p)) {
            Some(p) => children.entry(p).or_default().push(b),
            None => roots.push(b),
        }
    }
    let sort = |run: &mut Vec<&Block>| run.sort_by_key(|b| (b.order, b.id));
    sort(&mut roots);
    for run in children.values_mut() {
        sort(run);
    }

    let mut out: Vec<(usize, &Block)> = Vec::with_capacity(blocks.len());
    let mut seen: HashMap<BlockId, ()> = HashMap::new();
    let mut stack: Vec<(usize, &Block)> = roots.iter().rev().map(|b| (0, *b)).collect();
    // depth-first, pre-order; the reversed stack keeps siblings in order
    while let Some((depth, block)) = stack.pop() {
        if seen.insert(block.id, ()).is_some() {
            continue;
        }
        out.push((depth, block));
        if let Some(kids) = children.get(&block.id) {
            for kid in kids.iter().rev() {
                stack.push((depth + 1, kid));
            }
        }
    }
    if out.len() != blocks.len() {
        let mut extra = blocks
            .iter()
            .filter(|b| !seen.contains_key(&b.id))
            .collect::<Vec<&Block>>();
        sort(&mut extra);
        out.extend(extra.into_iter().map(|b| (0, b)));
    }
    out
}
