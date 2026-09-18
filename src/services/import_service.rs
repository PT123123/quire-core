// Markdown import (SPEC §二十六): CommonMark text -> a change list that
// creates one new page.
//
// The importer covers exactly the M4 block set (headings 1-3, bullet,
// numbered, todo, quote, code, divider, paragraph). Anything it does not
// recognize — inline emphasis, tables, images, setext headings — is kept as
// literal paragraph text, which is what makes `export_page(import(x))`
// semantically stable: text we cannot represent is never thrown away.
//
// Scope notes:
// - nested list indentation is recognized and flattened one level (the M4
//   editor only renders top-level blocks)
// - `####`..`######` become heading_3 (the block set has three levels)
// - code fences may carry an info string (```rs); it is dropped, the model
//   has no language field yet
// - block ids come from the caller's allocator: ids belong to the Document
//   generator, services never invent them (ADR-0012)

use crate::core::persistence::Change;
use crate::core::types::{Block, BlockId, BlockKind, OrderKey, Page};

/// One parsed block, before ids and order keys exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlock {
    pub kind: BlockKind,
    pub text: String,
    pub checked: bool,
}

/// Parse Markdown into the page's block list (display order).
pub fn parse_markdown(src: &str) -> Vec<ParsedBlock> {
    let mut out: Vec<ParsedBlock> = Vec::new();
    // `str::lines()` already normalizes CRLF, so a Windows file imports as
    // is; a lone '\r' (old Mac) is treated as ordinary text.
    let mut fence: Option<Vec<String>> = None;
    for raw in src.lines() {
        let body = raw.trim_start();
        if let Some(code) = fence.as_mut() {
            match fence_close_marker(body) {
                Some(_) => {
                    let taken = std::mem::take(code);
                    out.push(ParsedBlock {
                        kind: BlockKind::Code,
                        text: taken.join("\n"),
                        checked: false,
                    });
                    fence = None;
                }
                None => code.push(body.to_string()),
            }
            continue;
        }
        if fence_open_marker(body).is_some() {
            fence = Some(Vec::new());
            continue;
        }
        if body.trim_end().is_empty() {
            continue;
        }
        out.push(classify(body.trim_end()));
    }
    // unterminated fence: keep what was collected rather than lose it
    if let Some(code) = fence {
        out.push(ParsedBlock {
            kind: BlockKind::Code,
            text: code.join("\n"),
            checked: false,
        });
    }
    out
}

/// The change list for one new page: `PageCreated` (the caller's `page`,
/// so title/parent/order come from the app) followed by one `BlockInserted`
/// per parsed block. Block ids are taken from `alloc`; sibling order keys
/// are chained from `OrderKey::FIRST`.
pub fn import_markdown(
    src: &str,
    page: &Page,
    alloc: &mut dyn FnMut() -> BlockId,
) -> Vec<Change> {
    let mut changes = vec![Change::PageCreated(page.clone())];
    let mut prev: Option<OrderKey> = None;
    for parsed in parse_markdown(src) {
        let order = OrderKey::between(prev, None).expect("order space exhausted");
        prev = Some(order);
        changes.push(Change::BlockInserted(Block {
            id: alloc(),
            page: page.id,
            parent: None,
            order,
            kind: parsed.kind,
            text: parsed.text,
            checked: parsed.checked,
            // inline-mark parsing lands with the rich-text import pass
            marks: Vec::new(),
        }));
    }
    changes
}

fn fence_open_marker(body: &str) -> Option<char> {
    let marker = body.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run = body.chars().take_while(|&c| c == marker).count();
    (run >= 3).then_some(marker)
}

/// A fence only closes on the same marker with nothing after it; an info
/// string (```rs) never closes its own fence.
fn fence_close_marker(body: &str) -> Option<char> {
    let marker = fence_open_marker(body)?;
    let rest = body.trim_start_matches(marker);
    rest.trim().is_empty().then_some(marker)
}

fn classify(line: &str) -> ParsedBlock {
    if is_divider(line) {
        return block(BlockKind::Divider, "");
    }
    if let Some((level, text)) = heading(line) {
        let kind = match level {
            1 => BlockKind::Heading1,
            2 => BlockKind::Heading2,
            _ => BlockKind::Heading3, // ####..###### collapse to h3
        };
        return block(kind, text);
    }
    if let Some(rest) = line.strip_prefix('>') {
        return block(BlockKind::Quote, rest.trim_start());
    }
    if let Some(markers) = list_marker_body(line, &['-', '*', '+']) {
        if let Some(inner) = markers.strip_prefix('[') {
            // "[ ] text" / "[x] text"; an empty "[]" reads as unchecked
            if let Some(close) = inner.find(']') {
                let flag = inner[..close].trim();
                if flag.chars().count() <= 1 {
                    return ParsedBlock {
                        kind: BlockKind::Todo,
                        text: inner[close + 1..].trim_start().to_string(),
                        checked: flag.eq_ignore_ascii_case("x"),
                    };
                }
            }
        }
        return block(BlockKind::Bullet, markers);
    }
    if let Some(rest) = ordered_marker_body(line) {
        return block(BlockKind::Numbered, rest);
    }
    block(BlockKind::Paragraph, line)
}

fn block(kind: BlockKind, text: &str) -> ParsedBlock {
    ParsedBlock {
        kind,
        text: text.to_string(),
        checked: false,
    }
}

/// `---`, `***` (3+ markers, optionally spaced); anything else is text.
fn is_divider(line: &str) -> bool {
    let chars: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3 && (chars.iter().all(|&c| c == '-') || chars.iter().all(|&c| c == '*'))
}

/// `#..###### ` followed by the text; requires the space (or end of line),
/// so "#1 issue" stays a paragraph.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &line[level..];
    match rest.strip_prefix(' ') {
        Some(text) => Some((level, text.trim_start())),
        None if rest.is_empty() => Some((level, "")),
        _ => None,
    }
}

/// Body after an unordered list marker; `None` unless the marker is followed
/// by whitespace or ends the line (so "-ish" stays a paragraph).
fn list_marker_body<'a>(line: &'a str, markers: &[char]) -> Option<&'a str> {
    let first = line.chars().next()?;
    if !markers.contains(&first) {
        return None;
    }
    let rest = &line[first.len_utf8()..];
    marker_tail(rest)
}

/// `12. item` / `3) item` -> "item".
fn ordered_marker_body(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = &line[digits..];
    let after = match rest.strip_prefix('.') {
        Some(t) => t,
        None => rest.strip_prefix(')')?,
    };
    marker_tail(after)
}

/// A marker only counts when whitespace (or nothing) follows it.
fn marker_tail(rest: &str) -> Option<&str> {
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(' ') || rest.starts_with('\t') {
        Some(rest.trim_start())
    } else {
        None
    }
}
