// Markdown export (SPEC §二十六): a page's blocks -> CommonMark text.
//
// Scope is the M4 block set. Inline marks (M6) are not modelled yet, so
// text is written verbatim — `**bold**` inside a block stays literal, which
// is exactly what `import_service` reads back, so the round trip is lossless.
//
// Layout, chosen so re-importing yields the same block list:
// - one block per line group, groups separated by a blank line
// - consecutive items of the same list kind stay tight (one Markdown list),
//   switching kind starts a new list and gets a blank line
// - child blocks are indented two spaces per depth (the importer accepts and
//   flattens that indentation)
// - the file ends with exactly one '\n'; an empty page exports to ""

use std::collections::HashMap;

use crate::core::types::{Block, BlockId, BlockKind};

/// Render a page's blocks as Markdown, in display order.
pub fn export_page(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut number = 0;
    let walk = display_order(blocks);
    for (i, (depth, block)) in walk.iter().enumerate() {
        if i > 0 && !same_list_run(walk[i - 1].1.kind, block.kind) {
            out.push('\n');
        }
        let indent = "  ".repeat(*depth);
        let rendered = render(block, &mut number);
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
    let text = block.text.as_str();
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
