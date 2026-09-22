// Markdown import (SPEC §二十六): CommonMark text -> a change list that
// creates one new page.
//
// The importer covers the M4 block set (headings 1-3, bullet, numbered,
// todo, quote, code, divider, paragraph) plus math (SPEC §三十七), and the M6
// inline marks (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`,
// `[text](url)`, `$…$`). What it does not
// recognize — tables, images, setext headings, footnotes — is kept as
// literal paragraph text, which is what makes `export_page(import(x))`
// semantically stable: text we cannot represent is never thrown away.
//
// Scope notes:
// - a `$$ … $$` fence and a `$…$` span hold a LaTeX subset as *source*; the
//   renderer (`core::math`) turns it into Unicode at paint time, and nothing
//   here parses inside it
// - a `$` opens a span only the way TeX delimiters work — followed by a
//   non-space, closed by a `$` preceded by a non-space — so "costs $5 and
//   $10" stays prose instead of becoming a formula
// - nested list indentation is recognized and flattened one level (the M4
//   editor only renders top-level blocks)
// - `####`..`######` become heading_3 (the block set has three levels)
// - code fences may carry an info string (```rs); it names the language the
//   block is coloured with, and one this build cannot lex is no colour at all
// - inside a code fence and inside a code span nothing is parsed: those runs
//   keep their markers as text, because `Mark` has no nesting to describe it
// - `\*` and friends un-escape ASCII punctuation, so text that *literally*
//   says `**bold**` survives the round trip (export escapes it again)
// - block ids come from the caller's allocator: ids belong to the Document
//   generator, services never invent them (ADR-0012)

use crate::core::persistence::Change;
use crate::core::types::{
    Block, BlockId, BlockKind, ColorKind, Lang, Mark, MarkKind, OrderKey, Page,
};

/// One parsed block, before ids and order keys exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlock {
    pub kind: BlockKind,
    pub text: String,
    pub checked: bool,
    /// Inline marks over `text` (M6), byte offsets, sorted by start.
    pub marks: Vec<Mark>,
    /// A code fence's info string, folded through `Lang`: what colours the
    /// block. `Plain` for every other kind, and for a language this build
    /// does not know.
    pub lang: Lang,
}

/// Parse Markdown into the page's block list (display order).
pub fn parse_markdown(src: &str) -> Vec<ParsedBlock> {
    let mut out: Vec<ParsedBlock> = Vec::new();
    // `str::lines()` already normalizes CRLF, so a Windows file imports as
    // is; a lone '\r' (old Mac) is treated as ordinary text.
    let mut fence: Option<(Lang, Vec<String>)> = None;
    let mut math: Option<Vec<String>> = None;
    for raw in src.lines() {
        let body = raw.trim_start();
        if fence.is_some() {
            if fence_close_marker(body).is_some() {
                let (lang, code) = fence.take().expect("just checked");
                out.push(code_block(lang, code));
            } else if let Some((_, code)) = fence.as_mut() {
                code.push(body.to_string());
            }
            continue;
        }
        if fence_open_marker(body).is_some() {
            fence = Some((fence_lang(body), Vec::new()));
            continue;
        }
        // `$$ … $$` is a formula's fence, and like a code fence its lines are
        // verbatim: `\alpha` must survive with one backslash, so no inline pass
        // runs over it.
        if let Some(lines) = math.as_mut() {
            if body.trim_end() == "$$" {
                let taken = std::mem::take(lines);
                out.push(ParsedBlock {
                    kind: BlockKind::Math,
                    text: taken.join("\n"),
                    checked: false,
                    marks: Vec::new(),
                    lang: Lang::Plain,
                });
                math = None;
            } else {
                lines.push(body.to_string());
            }
            continue;
        }
        if body.trim_end() == "$$" {
            math = Some(Vec::new());
            continue;
        }
        if body.len() > 4 && body.starts_with("$$") && body.ends_with("$$") {
            out.push(ParsedBlock {
                kind: BlockKind::Math,
                text: body[2..body.len() - 2].trim().to_string(),
                checked: false,
                marks: Vec::new(),
                lang: Lang::Plain,
            });
            continue;
        }
        if body.trim_end().is_empty() {
            continue;
        }
        out.push(classify(body.trim_end()));
    }
    // unterminated fence: keep what was collected rather than lose it
    if let Some((lang, code)) = fence {
        out.push(code_block(lang, code));
    }
    if let Some(src) = math {
        out.push(ParsedBlock {
            kind: BlockKind::Math,
            text: src.join("\n"),
            checked: false,
            marks: Vec::new(),
            lang: Lang::Plain,
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
            folded: false,
            attachment: None,
            img_percent: 100,
            columns: 0,
            lang: parsed.lang,
            db_ref: None,
            sync_ref: None,        }));
    }
    changes
}

/// The rich-paste gate (SPEC §二十七): does the clipboard text carry block
/// structure worth landing as separate blocks? Plain text — a single
/// paragraph without markers — stays the TextInput's native plain paste,
/// which inserts at the caret without disturbing the block layout. Inline
/// marks alone do not trigger it (pasting `**bold**` mid-sentence should
/// stay literal), but a lone heading/list/todo/quote/code line does.
/// Returns the parsed blocks so the caller does not parse twice.
pub fn parse_if_block_structure(text: &str) -> Option<Vec<ParsedBlock>> {
    let parsed = parse_markdown(text);
    let structural = parsed.len() > 1
        || parsed.first().is_some_and(|b| {
            b.kind != BlockKind::Paragraph || b.checked
        });
    if structural {
        Some(parsed)
    } else {
        None
    }
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

/// What follows an opening fence's markers — the `rs` of "```rs" — as the
/// language to colour the block with. No info string, and an unknown one, are
/// both `Plain`: the text is what matters, and a language this build cannot
/// lex must not cost a reader its code.
fn fence_lang(body: &str) -> Lang {
    let Some(marker) = fence_open_marker(body) else {
        return Lang::Plain;
    };
    let info = body.trim_start_matches(marker);
    Lang::try_from_str(info).unwrap_or(Lang::Plain)
}

/// A code block from the lines its fence held. Verbatim source, so no marks,
/// ever — and the only kind whose parsed form carries a language.
fn code_block(lang: Lang, lines: Vec<String>) -> ParsedBlock {
    ParsedBlock {
        kind: BlockKind::Code,
        text: lines.join("\n"),
        checked: false,
        marks: Vec::new(),
        lang,
    }
}

/// One address and nothing else, which is the shape an embed card exports as.
fn is_bare_address(line: &str) -> bool {
    let t = line.trim();
    (t.starts_with("http://") || t.starts_with("https://"))
        && !t.chars().any(char::is_whitespace)
}

fn classify(line: &str) -> ParsedBlock {
    // The marker `export_page` writes for a contents block. It is checked
    // before anything else because it is the only line shape that is a *block
    // kind* rather than text: the contents themselves are derived, so this is
    // the whole of what survives an export.
    if line.trim() == "<!-- quire:toc -->" {
        return block(BlockKind::Toc, "");
    }
    // **A database has no marker and no arm here, and that is the decision**
    // (ADR-0065): it exports as a GitHub-flavored table — a real representation
    // a human and another tool can both use — and a table of pipe-separated
    // lines comes back as paragraphs, because a pipe line is text and nothing in
    // this reader pretends otherwise. The simple grid (ADR-0031) is in exactly
    // the same position, with the same test pinning it, which is why this is one
    // sentence here and not a second table parser: schema, property types, record
    // identities and views do not survive the content channel, by decision
    // rather than by omission.
    // A line that is nothing but an address reads back as the card that wrote
    // it. Deliberately narrow — one token, an explicit scheme — so a paragraph
    // that merely starts with a url stays a paragraph, and the text is taken
    // verbatim because there is no Markdown inside an address to eat.
    if is_bare_address(line) {
        return block(BlockKind::Embed, line.trim());
    }
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
                        lang: Lang::Plain,
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
        lang: Lang::Plain,
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
        MarkKind::Math => 5,
        // must match `export_service::kind_order`, or a document the app wrote
        // would come back with its atoms in a different order: both reference
        // kinds sort last because they are atoms and an outer-first order is
        // what the writer's boundary walk produces
        MarkKind::Mention => 6,
        MarkKind::Date => 7,
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
        // The reference layer's two inline atoms (SPEC §四十; ADR-0050).
        // Both are spelled `@[…]`, and what follows the closing bracket is what
        // tells them apart: a `(target)` makes it a mention, nothing does makes
        // it a date. The *syntax* decides and never the label, so a page that
        // really is called "2026-09-22" still reads back as a mention.
        //
        // This has to come before the generic `[…](…)` branch below: without it
        // `@` is ordinary text and the label is read as a *link* to
        // `quire://page/12`, which round-trips as a link and silently loses the
        // fact that the span was a reference at all.
        if c == '@' && bytes.get(i) == Some(&b'[') {
            if let Some((label_end, close)) = link_bounds(bytes, i) {
                let url = src[label_end + 2..close].trim();
                if url.starts_with(PAGE_REF_PREFIX) {
                    let start = out.len();
                    out.push_str(&src[at + 2..label_end]);
                    let end = out.len();
                    marks.push(Mark {
                        start,
                        end,
                        kind: MarkKind::Mention,
                        url: url.to_string(),
                        date: None,
                    });
                    i = close + 1;
                    continue;
                }
            }
            if let Some(label_end) = bracket_close(bytes, i) {
                let label = &src[at + 2..label_end];
                if crate::core::date::is_iso_date(label) {
                    let start = out.len();
                    out.push_str(label);
                    let end = out.len();
                    marks.push(Mark {
                        start,
                        end,
                        kind: MarkKind::Date,
                        url: String::new(),
                        date: Some(label.to_string()),
                    });
                    i = label_end + 1;
                    continue;
                }
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
                        date: None,
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
                        date: None,
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
                            date: None,
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
        // `$x$`: one dollar that both opens and closes around a non-space-
        // flanked run. The content is verbatim — a formula is source, and
        // un-escaping `\,` inside it would change the LaTeX.
        if c == '$' && run_len(bytes, at, b'$') == 1 && !src[i..].starts_with(char::is_whitespace) {
            if let Some(close) = math_closer(bytes, i) {
                let start = out.len();
                out.push_str(&src[i..close]);
                let end = out.len();
                marks.push(Mark {
                    start,
                    end,
                    kind: MarkKind::Math,
                    url: String::new(),
                    date: None,
                });
                i = close + 1;
                continue;
            }
        }
        out.push(c);
    }
}

/// The byte index of the `$` closing an inline formula started before `from`:
/// the first one with a non-space character in front of it, which is the rule
/// that keeps "costs $5 and $10" prose. Escaped dollars are skipped.
fn math_closer(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j < bytes.len() {
        j += escape_len(bytes, j);
        if j >= bytes.len() {
            return None;
        }
        if bytes[j] == b'$' && j > from && !bytes[j - 1].is_ascii_whitespace() {
            return Some(j);
        }
        j += 1;
    }
    None
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
            date: None,
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
/// The address a mention carries, borrowed from the one module that knows
/// its spelling (`core::reference`). ADR-0026 settled this form for a
/// block-level reference and an inline one is the same reference one level
/// down — which is also what makes a mention readable by the same jump path a
/// `quire://page` link already uses.
pub use crate::core::reference::PAGE_SCHEME as PAGE_REF_PREFIX;

/// The `]` closing a `[….]` opened at `at`, or `None`. Deliberately the
/// simplest possible scan — it exists for the *date* atom, whose spelling has
/// no `(target)` and therefore cannot go through `link_bounds`.
fn bracket_close(bytes: &[u8], at: usize) -> Option<usize> {
    let mut j = at + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b']' => return Some(j),
            _ => j += 1,
        }
    }
    None
}

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
