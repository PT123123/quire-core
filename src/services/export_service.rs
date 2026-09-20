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

use crate::core::types::{Block, BlockId, BlockKind, Mark, MarkKind};

/// Render a page's blocks as Markdown, in display order.
pub fn export_page(blocks: &[Block]) -> String {
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
                render_table(block, grids.get(&block.id).map(Vec::as_slice).unwrap_or(&[]))
            }
            _ => render(block, &mut number),
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
fn render_table(table: &Block, cells: &[Block]) -> String {
    let cols = table.columns as usize;
    if cols == 0 || cells.is_empty() {
        return String::new();
    }
    let cell = |b: Option<&Block>| {
        b.map(|c| {
            let text = render_inline(&c.text, &c.marks);
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
fn render(block: &Block, number: &mut usize) -> String {
    // a code block is source and a divider has no text: markers stay literal
    let text = match block.kind {
        BlockKind::Code | BlockKind::Divider => block.text.clone(),
        _ => render_inline(&block.text, &block.marks),
    };
    let text = text.as_str();
    match block.kind {
        BlockKind::Paragraph => {
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
            format!("```\n{text}\n```")
        }
        BlockKind::Divider => {
            *number = 0;
            "---".to_string()
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

fn render_inline(text: &str, marks: &[Mark]) -> String {
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
        // inside a code span the text is verbatim: escaping there would put
        // the backslash in the document
        if open.iter().any(|m| m.kind == MarkKind::Code) {
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
            // `` `a``b` `` reads back as a single span holding two backticks
            Some(last) if last.kind == m.kind && last.url == m.url && last.end == m.start => {
                last.end = m.end;
            }
            _ => joined.push(m),
        }
    }
    // a code span's content is literal, so nothing inside it can be styled
    let code: Vec<(usize, usize)> = joined
        .iter()
        .filter(|m| m.kind == MarkKind::Code)
        .map(|m| (m.start, m.end))
        .collect();
    joined.retain(|m| {
        m.kind == MarkKind::Code
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
    });
    out.push(Mark {
        start: at,
        end: m.end,
        kind: m.kind,
        url: m.url.clone(),
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
