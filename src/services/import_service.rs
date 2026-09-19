// Markdown import (SPEC §二十六): CommonMark text -> a change list that
// creates one new page.
//
// The importer covers exactly the M4 block set (headings 1-3, bullet,
// numbered, todo, quote, code, divider, paragraph) and the M6 inline marks
// (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`, `[text](url)`). What it
// does not recognize — tables, images, setext headings, footnotes — is kept as
// literal paragraph text, which is what makes `export_page(import(x))`
// semantically stable: text we cannot represent is never thrown away.
//
// Scope notes:
// - nested list indentation is recognized and flattened one level (the M4
//   editor only renders top-level blocks)
// - `####`..`######` become heading_3 (the block set has three levels)
// - code fences may carry an info string (```rs); it is dropped, the model
//   has no language field yet
// - inside a code fence and inside a code span nothing is parsed: those runs
//   keep their markers as text, because `Mark` has no nesting to describe it
// - `\*` and friends un-escape ASCII punctuation, so text that *literally*
//   says `**bold**` survives the round trip (export escapes it again)
// - block ids come from the caller's allocator: ids belong to the Document
//   generator, services never invent them (ADR-0012)

use crate::core::persistence::Change;
use crate::core::types::{Block, BlockId, BlockKind, ColorKind, Mark, MarkKind, OrderKey, Page};

/// One parsed block, before ids and order keys exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlock {
    pub kind: BlockKind,
    pub text: String,
    pub checked: bool,
    /// Inline marks over `text` (M6), byte offsets, sorted by start.
    pub marks: Vec<Mark>,
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
                        // a code block is verbatim source: no marks, ever
                        marks: Vec::new(),
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
            marks: Vec::new(),
        });
    }
    out
}

/// The change list for one new page: `PageCreated` (the caller's `page`,
/// so title/parent/order come from the app) followed by one `BlockInserted`
/// per parsed block. Block ids are taken from `alloc`; sibling order keys
/// are chained from `OrderKey::FIRST`.
pub fn import_markdown(src: &str, page: &Page, alloc: &mut dyn FnMut() -> BlockId) -> Vec<Change> {
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
            marks: parsed.marks,
            color: ColorKind::Default,
            background: ColorKind::Default,
            page_ref: None,
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
                    let (text, marks) = parse_inline(inner[close + 1..].trim_start());
                    return ParsedBlock {
                        kind: BlockKind::Todo,
                        text,
                        checked: flag.eq_ignore_ascii_case("x"),
                        marks,
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
    let (text, marks) = parse_inline(text);
    ParsedBlock {
        kind,
        text,
        checked: false,
        marks,
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

// ── inline marks (M6) ───────────────────────────────────────────────
//
// One recursive pass, no AST: a marker only counts when both its opener and
// its closer satisfy the flanking rules below, and everything between them is
// parsed again, which is what makes `**a _b_ c**` come out as bold with an
// italic inside. `Mark` is a flat span list, so nesting is expressed by the
// ranges themselves — the inner span sits inside the outer one, which is
// exactly the shape the exporter writes back.

/// Text plus marks for one inline run. Offsets are bytes into the *returned*
/// text (the markers are gone by then) and land on char boundaries, which is
/// the convention `Command::ToggleMark` and the renderer share.
pub fn parse_inline(src: &str) -> (String, Vec<Mark>) {
    let mut text = String::new();
    let mut marks = Vec::new();
    inline(src, &mut text, &mut marks);
    marks.sort_by_key(|m| (m.start, m.end, mark_order(m.kind)));
    marks.dedup_by(|a, b| a.kind == b.kind && a.start == b.start && a.end == b.end);
    (text, marks)
}

/// Ties between same-kind spans keep the order the editor sorts them in.
fn mark_order(kind: MarkKind) -> u8 {
    match kind {
        MarkKind::Bold => 0,
        MarkKind::Italic => 1,
        MarkKind::Strike => 2,
        MarkKind::Code => 3,
        MarkKind::Link => 4,
    }
}

fn inline(src: &str, out: &mut String, marks: &mut Vec<Mark>) {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < src.len() {
        let c = src[i..].chars().next().unwrap_or('\0');
        let at = i;
        i += c.len_utf8();
        if c == '\\' {
            // `\*` and friends: the backslash goes, the punctuation stays.
            match src[i..].chars().next() {
                Some(escaped) if escaped.is_ascii_punctuation() => {
                    out.push(escaped);
                    i += escaped.len_utf8();
                }
                _ => out.push(c),
            }
            continue;
        }
        if c == '`' {
            let run = run_len(bytes, at, b'`');
            if let Some(close) = code_closer(bytes, at + run, run) {
                code_span(out, marks, &src[at + run..close]);
                i = close + run;
                continue;
            }
        }
        if c == '[' {
            if let Some((label_end, close)) = link_bounds(bytes, at) {
                let url = src[label_end + 2..close].trim();
                let url = url.strip_prefix('<').unwrap_or(url);
                let url = url.strip_suffix('>').unwrap_or(url);
                let start = out.len();
                inline(&src[at + 1..label_end], out, marks);
                let end = out.len();
                if end > start {
                    marks.push(Mark {
                        start,
                        end,
                        kind: MarkKind::Link,
                        url: url.to_string(),
                    });
                }
                i = close + 1;
                continue;
            }
        }
        // a single `~` is text: strike needs two markers on each side
        if c == '~' && run_len(bytes, at, b'~') >= 2 {
            let n = 2;
            if let Some(close) = plain_closer(bytes, at, n, b'~') {
                let start = out.len();
                inline(&src[at + n..close], out, marks);
                let end = out.len();
                if end > start {
                    marks.push(Mark {
                        start,
                        end,
                        kind: MarkKind::Strike,
                        url: String::new(),
                    });
                }
                i = close + n;
                continue;
            }
        }
        if c == '*' || c == '_' {
            let marker = c as u8;
            let run = run_len(bytes, at, marker);
            // three markers are strong *and* emphasised, so the widest reading
            // is tried first and falls back to `**` / `*` when it finds no
            // closer of its own length
            let mut next = None;
            for n in [3usize, 2, 1].into_iter().filter(|&n| n <= run) {
                let Some(close) = emphasis_closer(bytes, at, n, marker) else {
                    continue;
                };
                let start = out.len();
                inline(&src[at + n..close], out, marks);
                let end = out.len();
                if end > start {
                    for kind in kinds_for_run(n) {
                        marks.push(Mark {
                            start,
                            end,
                            kind: *kind,
                            url: String::new(),
                        });
                    }
                }
                next = Some(close + n);
                break;
            }
            if let Some(next) = next {
                i = next;
                continue;
            }
        }
        out.push(c);
    }
}

/// `***x***` carries both styles over one span; the model has no combined
/// kind, so the two ranges simply coincide.
fn kinds_for_run(n: usize) -> &'static [MarkKind] {
    match n {
        0 | 1 => &[MarkKind::Italic],
        2 => &[MarkKind::Bold],
        _ => &[MarkKind::Bold, MarkKind::Italic],
    }
}

fn run_len(bytes: &[u8], at: usize, marker: u8) -> usize {
    bytes[at..].iter().take_while(|&&b| b == marker).count()
}

/// Length of a backslash escape at `j`, or 0 when nothing is escaped there.
/// The marker scans below must step over `\*` and friends: an escaped marker
/// is text, and `export_service` writes every literal marker that way.
fn escape_len(bytes: &[u8], j: usize) -> usize {
    if bytes[j] != b'\\' {
        return 0;
    }
    usize::from(bytes.get(j + 1).is_some_and(|b| b.is_ascii_punctuation())) * 2
}

/// A code span closes on a backtick run of exactly the opening length, which
/// is what lets a doubled fence hold a single backtick.
fn code_closer(bytes: &[u8], from: usize, run: usize) -> Option<usize> {
    let mut j = from;
    while j < bytes.len() {
        if bytes[j] != b'`' {
            j += 1;
            continue;
        }
        let r = run_len(bytes, j, b'`');
        if r == run {
            return (j > from).then_some(j);
        }
        j += r;
    }
    None
}

/// A code span's content is verbatim: the text goes in as it stands and only
/// the span itself is marked. CommonMark strips one space from each side when
/// the content both begins and ends with a space and holds something besides
/// spaces — that is the padding `export_service` adds so a span that starts
/// with a backtick cannot merge into its own fence.
fn code_span(out: &mut String, marks: &mut Vec<Mark>, raw: &str) {
    let content = match raw.strip_prefix(' ').and_then(|s| s.strip_suffix(' ')) {
        Some(inner) if !inner.trim().is_empty() => inner,
        _ => raw,
    };
    let start = out.len();
    out.push_str(content);
    let end = out.len();
    if end > start {
        marks.push(Mark {
            start,
            end,
            kind: MarkKind::Code,
            url: String::new(),
        });
    }
}

/// `~~strike~~` needs nothing beyond a non-empty run between two `~~`: `~`
/// carries no other meaning in this model, so there is nothing to disambiguate.
fn plain_closer(bytes: &[u8], open: usize, n: usize, marker: u8) -> Option<usize> {
    let content_start = open + n;
    let mut j = content_start;
    while j < bytes.len() {
        let skip = escape_len(bytes, j);
        if skip > 0 {
            j += skip;
            continue;
        }
        if bytes[j] != marker {
            j += 1;
            continue;
        }
        let run = run_len(bytes, j, marker);
        if run == n && j > content_start {
            return Some(j);
        }
        j += run;
    }
    None
}

/// Where an emphasis run of `n` markers opened at `open` closes, if it may.
/// CommonMark flanking, pared down to what the editor can type: an opener must
/// be followed by non-space, a closer must be preceded by non-space, and `_`
/// may not open or close inside a word — which is what keeps `snake_case` and
/// `2 * 3 * 4` as plain text.
fn emphasis_closer(bytes: &[u8], open: usize, n: usize, marker: u8) -> Option<usize> {
    match bytes.get(open + n) {
        Some(&b) if !b.is_ascii_whitespace() => {}
        _ => return None,
    }
    if marker == b'_' && open > 0 && bytes[open - 1].is_ascii_alphanumeric() {
        return None;
    }
    let mut j = open + n;
    while j < bytes.len() {
        let skip = escape_len(bytes, j);
        if skip > 0 {
            j += skip;
            continue;
        }
        if bytes[j] != marker {
            j += 1;
            continue;
        }
        let run = run_len(bytes, j, marker);
        let after_ok = bytes
            .get(j + n)
            .is_none_or(|&b| marker != b'_' || !b.is_ascii_alphanumeric());
        // a longer run may close a shorter opener: the exporter leans on this
        // when two spans touch, and the leftover markers open the next one.
        // The closer must leave content behind, or the markers are text.
        if run >= n && j > open + n && after_ok && !bytes[j - 1].is_ascii_whitespace() {
            return Some(j);
        }
        j += run;
    }
    None
}

/// For a `[` at `at`: the index of the `]` and of the closing `)`, or `None`
/// when this bracket is not a link. `\]` is escaped, a nested bracket pair
/// inside the label is skipped over, and the target may hold parentheses.
fn link_bounds(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut j = at + 1;
    let mut depth = 0usize;
    let label_end = loop {
        if j >= bytes.len() {
            return None;
        }
        match bytes[j] {
            b'\\' => j += 2,
            b'[' => {
                depth += 1;
                j += 1;
            }
            b']' if depth > 0 => {
                depth -= 1;
                j += 1;
            }
            b']' => break j,
            _ => j += 1,
        }
    };
    if bytes.get(label_end + 1) != Some(&b'(') {
        return None;
    }
    let mut depth = 1usize;
    let mut k = label_end + 2;
    let close = loop {
        if k >= bytes.len() {
            return None;
        }
        match bytes[k] {
            b'\\' => k += 2,
            b'(' => {
                depth += 1;
                k += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break k;
                }
                k += 1;
            }
            _ => k += 1,
        }
    };
    Some((label_end, close))
}
