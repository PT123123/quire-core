// Lexical colour for code blocks (SPEC §三十七 批次 C).
//
// No tree-sitter, no WASM, no regex: SPEC asks for 纯词法着色 and §二/§三十三 rule
// out a JS runtime, so what a code block gets is one character scanner per
// language that answers a single question about every character of the source —
// which of six colours it is.
//
// The scanner is half of it. `layer` is the other half, and it is the reason the
// scanner is affordable: it returns the *whole block* as one string, with every
// character that is not the requested colour replaced by a no-break space (its
// whitespace left alone). A row then paints its colours as Text elements that
// are the same length and break at the same hard newlines, so they sit on each
// other glyph for glyph: no layout engine has to agree with a second one, and
// the Text that measures the row is one of them. The price is the assumption
// that the code font is monospace, which is what makes a blank column and a
// letter column the same width.
//
// The newlines `layer` inserts are derived and never stored. A block's text is
// the source as typed; the column budget is the row's own width over the font's
// advance, which only the UI knows.

use crate::core::types::Lang;

/// Which colour a character gets. The ints are the Slint side's `kind`, so the
/// order is load-bearing: 0 is what the block's own text colour paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Identifiers, punctuation, operators: the block's own colour.
    Plain,
    /// A reserved word of the language.
    Keyword,
    /// A comment, or a Markdown blockquote line. Drawn last, so it is also the
    /// colour a character the model cannot blank takes (see `build`).
    Comment,
    /// A string literal, or a Markdown code span.
    Literal,
    /// A number.
    Number,
    /// The name of a thing: a type, a decorator, a shell variable, a JSON key,
    /// a Markdown heading.
    Label,
}

/// Colours a row can ask for, and the count the `for` in the delegate repeats.
pub const KIND_COUNT: i32 = 6;

impl Kind {
    /// The int the delegate passes back to the callback. The five it paints over
    /// a code block are drawn in the order 1, 3, 4, 5, 2 — comment last, because
    /// a character that cannot be blanked is copied into every layer and ends up
    /// painted by whichever layer is on top, and prose is what is most likely to
    /// be unblankable (see `build`).
    pub fn from_int(k: i32) -> Kind {
        match k {
            1 => Kind::Keyword,
            2 => Kind::Comment,
            3 => Kind::Literal,
            4 => Kind::Number,
            5 => Kind::Label,
            _ => Kind::Plain,
        }
    }
}

/// The character a blank column is made of: invisible, one advance wide like
/// every other column of a monospace font, and — which is why it is this
/// character and not a space — not a place a line is allowed to break.
const BLANK: char = '\u{a0}';

/// The whitespace the scanner and the wrapper agree on. `BLANK` is not one of
/// them: it is paint, not structure.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}')
}

fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\u{2028}' | '\u{2029}')
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// One colour of a code block, as a row paints it. `Kind::Plain` is the colour
/// the row measures itself with, so it is the one that must never lose a
/// character: blanks for it are the same width as the letters they hide.
///
/// `frame` and `advance` are in pixels: the row measures its own width and the
/// font's advance, and their quotient is how many characters fit on a line. An
/// `advance` of 0 means the row has not measured the font yet, and the answer is
/// the blanking without any added newlines — the block soft-wraps the way an
/// unhighlighted one does, which is a plainer rendering, not a broken one.
pub fn layer(text: &str, lang: Lang, frame: f32, advance: f32, kind: Kind) -> String {
    let chars: Vec<char> = text.chars().collect();
    let kinds = colours(&chars, lang);
    let cuts = cuts(&chars, columns(frame, advance));
    build(&chars, &kinds, kind, &cuts)
}

fn columns(frame: f32, advance: f32) -> usize {
    if advance > 0.0 && frame > 0.0 {
        (f64::from(frame) / f64::from(advance)).floor().max(1.0) as usize
    } else {
        0
    }
}

/// Every character's colour, indexed by character (not byte: a column is a
/// character, and the wrapper counts columns).
fn colours(chars: &[char], lang: Lang) -> Vec<Kind> {
    let mut kinds = vec![Kind::Plain; chars.len()];
    if lang == Lang::Plain {
        return kinds;
    }
    let mut lex = Lex {
        chars,
        kinds: &mut kinds,
    };
    match lang {
        Lang::Rust => lex.c_like(true),
        Lang::Js | Lang::Ts => lex.c_like(false),
        Lang::Python => lex.python(),
        Lang::Json => lex.json(),
        Lang::Bash => lex.bash(),
        Lang::Md => lex.markdown(),
        _ => {}
    }
    kinds
}

/// Where a visual line starts: put a newline at `at`, and drop the columns
/// `drop_to..at` — the whitespace a soft wrap swallows at a break. The two are
/// equal for a hard break inside a word that is longer than a line by itself,
/// where there is no whitespace to swallow.
struct Cut {
    at: usize,
    drop_to: usize,
}

struct Lex<'a> {
    chars: &'a [char],
    kinds: &'a mut [Kind],
}

impl<'a> Lex<'a> {
    fn mark(&mut self, from: usize, to: usize, kind: Kind) {
        if let Some(slice) = self.kinds.get_mut(from..to) {
            slice.fill(kind);
        }
    }

    fn get(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    fn starts_at(&self, i: usize, needle: &str) -> bool {
        needle
            .chars()
            .enumerate()
            .all(|(k, c)| self.get(i + k) == Some(c))
    }

    /// The identifier-ish run from `i`; its length.
    fn run(&self, i: usize) -> usize {
        let mut j = i;
        while matches!(self.get(j), Some(c) if is_ident(c)) {
            j += 1;
        }
        j - i
    }

    fn matches_word(&self, from: usize, len: usize, word: &str) -> bool {
        word.chars().count() == len && self.starts_at(from, word)
    }

    fn any_of(&self, from: usize, len: usize, list: &[&str]) -> bool {
        list.iter().any(|w| self.matches_word(from, len, w))
    }

    fn capitalised(&self, from: usize) -> bool {
        self.get(from).is_some_and(|c| c.is_uppercase())
    }

    /// `//` comments, and the C block comments — Rust's nest, C's do not.
    fn c_like(&mut self, rust: bool) {
        let keywords: &[&str] = if rust { RUST_KEYWORDS } else { JS_KEYWORDS };
        let mut i = 0;
        while i < self.chars.len() {
            let c = self.chars[i];
            if c == '/' && self.starts_at(i, "//") {
                i = self.till_newline(i);
                continue;
            }
            if c == '/' && self.starts_at(i, "/*") {
                i = self.block_comment(i, rust);
                continue;
            }
            if c == '"' || (!rust && matches!(c, '\'' | '`')) {
                i = self.quoted(i, c, true);
                continue;
            }
            if rust && c == '\'' {
                // A character literal is `'x'` or `'\x'`; a tick that opens
                // anything else is a lifetime, and a lifetime is a name. The
                // distinction matters: a scan for a closing tick that never
                // finds one would colour the rest of the line as a string.
                let closes = self.get(i + 2) == Some('\'')
                    || (self.get(i + 1) == Some('\\') && self.get(i + 3) == Some('\''));
                if closes {
                    i = self.quoted(i, '\'', true);
                } else {
                    let len = self.run(i + 1);
                    self.mark(i, i + 1 + len, Kind::Label);
                    i += 1 + len;
                }
                continue;
            }
            if rust && c == '#' && self.starts_at(i, "#[") {
                let end = self.bracket_end(i + 1, '[', ']');
                self.mark(i, end, Kind::Label);
                i = end;
                continue;
            }
            if !rust && (c == '@' || c == '$') {
                let len = self.run(i + if c == '@' { 1 } else { 0 });
                self.mark(i, i + len, Kind::Label);
                i += len;
                continue;
            }
            if c.is_ascii_digit() {
                let len = self.number(i);
                self.mark(i, i + len, Kind::Number);
                i += len;
                continue;
            }
            if is_ident_start(c) {
                let len = self.run(i);
                let macro_call = rust && self.get(i + len) == Some('!');
                let kind = if self.any_of(i, len, keywords) {
                    Kind::Keyword
                } else if macro_call || self.capitalised(i) {
                    Kind::Label
                } else {
                    Kind::Plain
                };
                self.mark(i, i + len, kind);
                i += len + macro_call as usize;
                continue;
            }
            i += 1;
        }
    }

    fn python(&mut self) {
        let mut i = 0;
        // The name after `def` or `class` is a name, not a keyword, so the
        // scanner has to remember that it just saw one.
        let mut declaring = false;
        while i < self.chars.len() {
            let c = self.chars[i];
            if c == '#' {
                i = self.till_newline(i);
                continue;
            }
            if matches!(c, '"' | '\'') && self.get(i + 1) == Some(c) && self.get(i + 2) == Some(c) {
                i = self.triple(i, c);
                continue;
            }
            if matches!(c, '"' | '\'') {
                i = self.quoted(i, c, true);
                continue;
            }
            if c == '@' {
                let len = self.run(i + 1);
                self.mark(i, i + 1 + len, Kind::Label);
                i += 1 + len;
                continue;
            }
            if c.is_ascii_digit() {
                let len = self.number(i);
                self.mark(i, i + len, Kind::Number);
                i += len;
                continue;
            }
            if is_ident_start(c) {
                let len = self.run(i);
                let keyword = self.any_of(i, len, PYTHON_KEYWORDS);
                let kind = if keyword {
                    Kind::Keyword
                } else if declaring || self.capitalised(i) || self.any_of(i, len, PYTHON_BUILTINS) {
                    Kind::Label
                } else {
                    Kind::Plain
                };
                self.mark(i, i + len, kind);
                declaring = keyword && self.any_of(i, len, &["def", "class"]);
                i += len;
                continue;
            }
            if is_line_break(c) {
                declaring = false;
            }
            i += 1;
        }
    }

    fn json(&mut self) {
        let mut i = 0;
        while i < self.chars.len() {
            let c = self.chars[i];
            if c == '"' {
                let end = self.quoted(i, '"', true);
                // A string with a colon after it is a key, and a key is a name;
                // every other string is a value.
                let mut j = end;
                while matches!(self.get(j), Some(c) if c == ' ' || c == '\t') {
                    j += 1;
                }
                let kind = if self.get(j) == Some(':') {
                    Kind::Label
                } else {
                    Kind::Literal
                };
                self.mark(i, end, kind);
                i = end;
                continue;
            }
            if c == '-' || c.is_ascii_digit() {
                let len = self.number(i);
                self.mark(i, i + len, Kind::Number);
                i += len;
                continue;
            }
            if is_ident_start(c) {
                let len = self.run(i);
                if self.any_of(i, len, &["true", "false", "null"]) {
                    self.mark(i, i + len, Kind::Keyword);
                }
                i += len;
                continue;
            }
            i += 1;
        }
    }

    fn bash(&mut self) {
        let mut i = 0;
        while i < self.chars.len() {
            let c = self.chars[i];
            // A `#` only opens a comment where a word could start: `a#b` is one
            // word, and the first line's `#!` is a comment like any other.
            let at_word_start = i == 0 || !is_ident(self.chars[i - 1]);
            if c == '#' && at_word_start {
                i = self.till_newline(i);
                continue;
            }
            if matches!(c, '\'' | '"' | '`') {
                i = self.quoted(i, c, c != '\'');
                continue;
            }
            if c == '$' && at_word_start {
                if self.get(i + 1) == Some('{') {
                    let end = self.bracket_end(i + 1, '{', '}');
                    self.mark(i, end, Kind::Label);
                    i = end;
                } else {
                    let len = self.run(i + 1);
                    self.mark(i, i + 1 + len, Kind::Label);
                    i += 1 + len;
                }
                continue;
            }
            if (c.is_ascii_digit() || c == '-') && at_word_start {
                let len = self.number(i);
                if len > 0 {
                    self.mark(i, i + len, Kind::Number);
                    i += len;
                    continue;
                }
            }
            if is_ident_start(c) && at_word_start {
                let len = self.run(i);
                if self.any_of(i, len, BASH_KEYWORDS) {
                    self.mark(i, i + len, Kind::Keyword);
                }
                i += len;
                continue;
            }
            i += 1;
        }
    }

    /// The line-oriented one, because Markdown is: a heading, a quote, a fence
    /// and what it fences, and the inline spans in between.
    fn markdown(&mut self) {
        let mut i = 0;
        let mut fence = '\0';
        while i < self.chars.len() {
            let end = self
                .chars
                .iter()
                .skip(i)
                .position(|c| is_line_break(*c))
                .map_or(self.chars.len(), |q| i + q);
            let body = i
                + self.chars[i..end]
                    .iter()
                    .take_while(|c| **c == ' ' || **c == '\t')
                    .count();
            let first = self.chars[body..end].first().copied();
            let marker = self
                .chars
                .iter()
                .skip(body)
                .take_while(|c| Some(**c) == first)
                .count();
            if fence != '\0' {
                // Inside a fence the text is code, and code is one colour. A
                // line of nothing but the fence character is what closes it.
                self.mark(i, end, Kind::Literal);
                if first == Some(fence) && marker == end - body {
                    fence = '\0';
                }
            } else if first == Some('#') && (1..=6).contains(&marker) {
                self.mark(i, end, Kind::Label);
            } else if first == Some('>') {
                self.mark(i, end, Kind::Comment);
            } else if matches!(first, Some('`') | Some('~')) && marker >= 3 {
                fence = first.unwrap_or('\0');
                self.mark(i, end, Kind::Literal);
            } else {
                self.mark_inline(i, end);
            }
            i = end + 1;
        }
    }

    /// Inline spans of one Markdown line: `` `code` `` is a string, and a link
    /// is a name around a url.
    fn mark_inline(&mut self, from: usize, to: usize) {
        let mut i = from;
        while i < to {
            match self.chars[i] {
                '`' => {
                    let close = self.chars[i + 1..to]
                        .iter()
                        .position(|c| *c == '`')
                        .map_or(to, |p| i + 1 + p);
                    self.mark(i, close.min(to) + 1, Kind::Literal);
                    i = close + 2;
                }
                '[' | '!' => {
                    let Some(bracket) = self.chars[i + 1..to]
                        .iter()
                        .position(|c| *c == ']')
                        .map(|p| i + 1 + p)
                    else {
                        i += 1;
                        continue;
                    };
                    if self.get(bracket + 1) != Some('(') {
                        i = bracket + 1;
                        continue;
                    }
                    let Some(paren) = self.chars[bracket + 1..to]
                        .iter()
                        .position(|c| *c == ')')
                        .map(|p| bracket + 1 + p)
                    else {
                        i = bracket + 1;
                        continue;
                    };
                    self.mark(i, bracket + 1, Kind::Label);
                    self.mark(bracket + 1, paren + 1, Kind::Literal);
                    i = paren + 1;
                }
                _ => i += 1,
            }
        }
    }

    fn till_newline(&mut self, from: usize) -> usize {
        let mut j = from;
        while matches!(self.get(j), Some(c) if !is_line_break(c)) {
            j += 1;
        }
        self.mark(from, j, Kind::Comment);
        j
    }

    fn block_comment(&mut self, from: usize, nested: bool) -> usize {
        let mut i = from + 2;
        let mut depth = 1usize;
        while i < self.chars.len() {
            if self.starts_at(i, "*/") {
                depth = depth.saturating_sub(1);
                i += 2;
                if depth == 0 {
                    break;
                }
                continue;
            }
            if nested && self.starts_at(i, "/*") {
                depth += 1;
                i += 2;
                continue;
            }
            i += 1;
        }
        self.mark(from, i, Kind::Comment);
        i
    }

    /// A quoted run opening at `i`. An escape eats the next character, and a
    /// string that never closes ends with its line rather than eating the file.
    fn quoted(&mut self, i: usize, quote: char, escapes: bool) -> usize {
        let mut j = i + 1;
        while j < self.chars.len() {
            let c = self.chars[j];
            if escapes && c == '\\' {
                j += 2;
                continue;
            }
            if c == quote {
                j += 1;
                break;
            }
            if is_line_break(c) && quote != '`' {
                break;
            }
            j += 1;
        }
        self.mark(i, j, Kind::Literal);
        j
    }

    fn triple(&mut self, i: usize, quote: char) -> usize {
        let mut j = i + 3;
        while j < self.chars.len() {
            if self.chars[j] == quote
                && self.get(j + 1) == Some(quote)
                && self.get(j + 2) == Some(quote)
            {
                j += 3;
                break;
            }
            j += 1;
        }
        self.mark(i, j, Kind::Literal);
        j
    }

    /// One past the closer of the bracket group that opens at `i`, or the end of
    /// the text when it never closes.
    fn bracket_end(&self, i: usize, open: char, close: char) -> usize {
        let mut depth = 0i32;
        let mut j = i;
        while j < self.chars.len() {
            match self.chars[j] {
                c if c == open => depth += 1,
                c if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        return j + 1;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        self.chars.len()
    }

    /// A number literal: digits, the radix and digit-separator letters, and the
    /// decimal point. Returns its length, 0 when `from` is not one.
    fn number(&self, from: usize) -> usize {
        let mut j = from;
        while matches!(self.get(j), Some(c) if c.is_ascii_alphanumeric() || c == '_' || (c == '.' && j > from)) {
            j += 1;
        }
        // `1.foo` is a number and a field; the point only belongs to the number
        // when a digit follows it.
        while j > from && self.get(j - 1) == Some('.') {
            j -= 1;
        }
        j - from
    }
}

/// Columns a character costs a line. ASCII is one: a monospace font lays out
/// its own repertoire one column per glyph. Anything else arrives from a
/// fallback font and may be anything up to a full em box, so it is budgeted as
/// two — over-budgeting is the safe direction, because a line that stays inside
/// its budget on this estimate can only be narrower than the frame, which is
/// what keeps the layout from ever needing a soft wrap (see `build`: the layers
/// may not wrap in places each other does not). A tab gets the widest stop any
/// layout is known to give it, for the same reason.
fn width(c: char) -> usize {
    match c {
        '\t' => 8,
        c if c.is_ascii() => 1,
        _ => 2,
    }
}

/// The newlines a row has to add so that nothing soft-wraps: a greedy fill per
/// source line, breaking at the whitespace in front of a word that no longer
/// fits, and inside a word that is longer than the line on its own.
fn cuts(chars: &[char], cols: usize) -> Vec<Cut> {
    if cols == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    // Columns spent on the current visual line, the pending whitespace run
    // included: a break drops that run, so this model and a soft wrap end up
    // spending the same columns for the same words.
    let mut used = 0;
    let mut ws: Option<usize> = None;
    while i < chars.len() {
        let c = chars[i];
        if is_line_break(c) {
            used = 0;
            ws = None;
            i += 1;
            continue;
        }
        if is_space(c) {
            ws.get_or_insert(i);
            used += width(c);
            i += 1;
            continue;
        }
        let word = i;
        let mut left = 0;
        while i < chars.len() && !is_space(chars[i]) {
            left += width(chars[i]);
            i += 1;
        }
        if used > 0 && used + left > cols {
            // The word moves down, and the whitespace in front of it *is* the
            // break: one newline for as many columns as there were.
            out.push(Cut {
                at: word,
                drop_to: ws.unwrap_or(word),
            });
            used = 0;
        }
        ws = None;
        let mut pos = word;
        while used + left > cols {
            let mut room = cols - used;
            let start = pos;
            while pos < i {
                let w = width(chars[pos]);
                if w > room {
                    break;
                }
                room -= w;
                left -= w;
                pos += 1;
            }
            if pos == start {
                // Nothing of the word fits in what is left of the line, which
                // can only mean a character wider than the row itself: it gets
                // a line of its own instead of being clipped.
                left -= width(chars[pos]);
                pos += 1;
            }
            out.push(Cut {
                at: pos,
                drop_to: pos,
            });
            used = 0;
        }
        used += left;
    }
    out
}

/// A kind's whole-block string: the real character where the column is of that
/// colour, is whitespace, or is not ASCII; `BLANK` everywhere else, with `cuts`
/// applied.
///
/// The non-ASCII clause is the one that is not obvious. A blank is only safe
/// when it is exactly as wide as the character it hides, and that is guaranteed
/// for a monospace font's ASCII and for nothing else: a CJK ideograph is a full
/// em in whatever fallback font supplies it, which is not a whole number of
/// columns in a Latin monospace font. So a character the model cannot measure is
/// copied into every layer instead — the layers stay aligned, which is what they
/// exist for, and the character takes the colour of whichever layer draws last
/// rather than its own. Alignment wins over colour: a colour that lands on the
/// wrong glyphs makes the block unreadable, a muted character does not.
fn build(chars: &[char], kinds: &[Kind], kind: Kind, cuts: &[Cut]) -> String {
    let mut out = String::with_capacity(chars.len() + cuts.len());
    let mut next = 0;
    let mut i = 0;
    while i < chars.len() {
        if let Some(cut) = cuts.get(next) {
            if cut.drop_to == i {
                out.push('\n');
                next += 1;
                i = cut.at;
                continue;
            }
        }
        let c = chars[i];
        out.push(if kinds[i] == kind || is_space(c) || !c.is_ascii() {
            c
        } else {
            BLANK
        });
        i += 1;
    }
    out
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "yield",
];

const JS_KEYWORDS: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "constructor", "continue",
    "debugger", "default", "delete", "do", "else", "enum", "export", "extends", "false",
    "finally", "for", "from", "function", "get", "if", "implements", "import", "in",
    "instanceof", "interface", "let", "new", "null", "of", "package", "private", "protected",
    "public", "return", "set", "static", "super", "switch", "this", "throw", "true", "try",
    "type", "typeof", "var", "void", "while", "with", "yield",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield", "True",
    "False", "None", "self", "cls",
];

const PYTHON_BUILTINS: &[&str] = &[
    "abs", "all", "any", "bool", "dict", "dir", "enumerate", "float", "format", "frozenset",
    "getattr", "hex", "id", "int", "isinstance", "iter", "len", "list", "map", "max", "min",
    "next", "object", "oct", "open", "print", "range", "repr", "reversed", "round", "set",
    "setattr", "slice", "sorted", "str", "sum", "super", "tuple", "type", "vars", "zip",
];

const BASH_KEYWORDS: &[&str] = &[
    "break", "case", "continue", "do", "done", "elif", "else", "esac", "fi", "for", "function",
    "if", "in", "local", "return", "select", "then", "time", "until", "while", "echo", "exit",
    "export", "set", "source", "shift", "trap", "unset", "read",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The colour of every character of a source line, one letter each: plain,
    /// keyword, comment, literal, number, Label.
    fn paint(src: &str, lang: Lang) -> String {
        let chars: Vec<char> = src.chars().collect();
        colours(&chars, lang)
            .iter()
            .map(|k| match k {
                Kind::Plain => 'p',
                Kind::Keyword => 'k',
                Kind::Comment => 'c',
                Kind::Literal => 'l',
                Kind::Number => 'n',
                Kind::Label => 'L',
            })
            .collect()
    }

    #[test]
    fn rust_gets_its_five_colours() {
        assert_eq!(
            paint("fn main() { let x = 3; }", Lang::Rust),
            "kkppppppppppkkkpppppnppp"
        );
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_a_url_not_a_comment() {
        let src = "let u = \"https://x\"; // tail";
        let p = paint(src, Lang::Rust);
        // The `//` of the url is part of the string; the one after the `;` is
        // the comment, and it runs to the end of the line.
        assert_eq!(&p[8..19], "lllllllllll");
        assert_eq!(&p[19..21], "pp");
        assert_eq!(&p[21..], "ccccccc");
    }

    #[test]
    fn block_comments_nest_only_where_they_nest() {
        let src = "/* a /* b */ c */ x";
        assert_eq!(paint(src, Lang::Rust).matches('c').count(), src.chars().count() - 2);
        // Without nesting the first closer wins, and what follows is code again.
        let p = paint(src, Lang::Js);
        assert_eq!(&p[..12], "cccccccccccc");
        assert_eq!(&p[12..], "ppppppp");
    }

    #[test]
    fn a_lifetime_is_a_name_and_a_char_literal_is_a_string() {
        // The tick of a lifetime must not swallow the line looking for a closer.
        let p = paint("fn f<'a>(x: &str) {}", Lang::Rust);
        assert_eq!(&p[5..7], "LL");
        assert_eq!(&p[7..8], "p");
        assert_eq!(p.matches('l').count(), 0);
        assert_eq!(paint("'c'", Lang::Rust), "lll");
        assert_eq!(paint("'\\n'", Lang::Rust), "llll");
    }

    #[test]
    fn json_tells_a_key_from_its_value() {
        assert_eq!(
            paint("{\"a\": \"b\", \"c\": 3}", Lang::Json),
            "pLLLpplllppLLLppnp"
        );
    }

    #[test]
    fn python_colours_a_definition_and_a_docstring() {
        let src = "def f(x):\n    return x";
        assert_eq!(&paint(src, Lang::Python)[..6], "kkkpLp");
        assert_eq!(&paint(src, Lang::Python)[14..20], "kkkkkk");
        assert_eq!(paint("\"\"\"hi\"\"\"", Lang::Python), "llllllll");
    }

    #[test]
    fn bash_variables_are_names_and_the_hash_in_a_word_is_not_a_comment() {
        let p = paint("echo $HOME a#b # real", Lang::Bash);
        assert_eq!(&p[..4], "kkkk");
        assert_eq!(&p[5..10], "LLLLL");
        assert_eq!(&p[10..15], "ppppp");
        assert_eq!(&p[15..], "cccccc");
    }

    #[test]
    fn markdown_headings_quotes_and_code() {
        assert_eq!(paint("## Title", Lang::Md), "LLLLLLLL");
        assert_eq!(&paint("see `x = 1` here", Lang::Md)[4..11], "lllllll");
        assert_eq!(paint("> quoted", Lang::Md), "cccccccc");
        assert_eq!(paint("```sh", Lang::Md), "lllll");
    }

    #[test]
    fn a_layer_blanks_everything_that_is_not_its_colour() {
        let src = "let n = 42";
        assert_eq!(
            layer(src, Lang::Rust, 0.0, 0.0, Kind::Number),
            "\u{a0}\u{a0}\u{a0} \u{a0} \u{a0} 42"
        );
        // Whitespace stays itself in every layer: the layers are only aligned if
        // their characters break like each other's, and a blanked space would be
        // one more class of character for the layout to disagree about.
        for kind in 0..KIND_COUNT {
            let s = layer(src, Lang::Rust, 0.0, 0.0, Kind::from_int(kind));
            assert_eq!(s.chars().count(), src.chars().count());
            for (i, c) in src.chars().enumerate() {
                if is_space(c) {
                    assert_eq!(s.chars().nth(i), Some(c), "kind {kind}");
                }
            }
        }
    }

    #[test]
    fn a_plain_block_paints_its_text_and_nothing_else() {
        let src = "let n = 42";
        assert_eq!(layer(src, Lang::Plain, 0.0, 0.0, Kind::Plain), src);
        assert_eq!(
            layer(src, Lang::Plain, 0.0, 0.0, Kind::Number),
            "\u{a0}\u{a0}\u{a0} \u{a0} \u{a0} \u{a0}\u{a0}"
        );
    }

    #[test]
    fn a_long_line_breaks_where_a_column_it_was_given_does() {
        assert_eq!(
            layer("aaaa bbbb cccc", Lang::Plain, 5.0, 1.0, Kind::Plain),
            "aaaa\nbbbb\ncccc"
        );
        // A word longer than the line is cut rather than clipped.
        assert_eq!(layer("abcdefgh", Lang::Plain, 3.0, 1.0, Kind::Plain), "abc\ndef\ngh");
        // No advance measured yet: no newlines added, and no clipping either —
        // the row falls back to the soft wrap an unhighlighted block uses.
        assert_eq!(layer("aaaa bbbb", Lang::Plain, 5.0, 0.0, Kind::Plain), "aaaa bbbb");
    }

    #[test]
    fn a_source_newline_is_content_and_the_indent_survives() {
        let src = "fn f() {\n    let x = 1\n}";
        // A wide row adds no breaks, so the block's own newlines are all it has.
        assert_eq!(layer(src, Lang::Rust, 60.0, 1.0, Kind::Plain).matches('\n').count(), 2);
        // Unhighlighted, a layer is the block's text itself: same string, same
        // height, and the stored source is what a row shows.
        assert_eq!(layer(src, Lang::Plain, 60.0, 1.0, Kind::Plain), src);
        // The indentation of a source line is whitespace, so it stays put.
        assert_eq!(
            layer("  let x", Lang::Rust, 0.0, 0.0, Kind::Keyword),
            "  let \u{a0}"
        );
    }

    #[test]
    fn every_kind_shares_the_breaks_so_the_layers_stack() {
        let src = "let alpha = \"a long string value\" // with a trailing comment\n    中文字 = 3 // 注释";
        for cols in 6..24usize {
            let (frame, advance) = (cols as f32, 1.0);
            let plain: Vec<String> = layer(src, Lang::Rust, frame, advance, Kind::Plain)
                .split('\n')
                .map(str::to_string)
                .collect();
            for kind in 0..KIND_COUNT {
                let s = layer(src, Lang::Rust, frame, advance, Kind::from_int(kind));
                let lines: Vec<&str> = s.split('\n').collect();
                assert_eq!(
                    lines.len(),
                    plain.len(),
                    "kind {kind} at {cols} columns wrapped differently"
                );
                for (a, b) in plain.iter().zip(&lines) {
                    assert_eq!(a.chars().count(), b.chars().count());
                    // Where two layers disagree, one of them is blanking an
                    // ASCII column. A real character never moves or changes.
                    for (ca, cb) in a.chars().zip(b.chars()) {
                        if ca != cb {
                            assert!(ca == BLANK || cb == BLANK, "{ca:?} vs {cb:?}");
                        }
                    }
                }
            }
        }
    }

    /// Columns a painted line costs: a blank stands in for the one ASCII column
    /// it replaced, which is the whole deal of the layering model. Trailing
    /// whitespace is not counted, because it is the one thing a line may overflow
    /// with and still be the same line in every layer.
    fn painted_width(line: &str) -> usize {
        line.trim_end_matches(|c| is_space(c))
            .chars()
            .map(|c| if c == BLANK { 1 } else { width(c) })
            .sum()
    }

    #[test]
    fn a_wrapped_line_never_exceeds_what_the_row_can_show() {
        let src =
            "const value = compute(a, b, c)\t+\tanother_identifier_here;\nsecond\tline here\n    中文宽字符 x = 1";
        for cols in 3..24usize {
            for kind in 0..KIND_COUNT {
                let wrapped = layer(src, Lang::Rust, cols as f32, 1.0, Kind::from_int(kind));
                for line in wrapped.split('\n') {
                    assert!(
                        painted_width(line) <= cols,
                        "{cols} columns cannot hold {line:?} as {kind:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_wide_character_is_copied_rather_than_blanked() {
        // A CJK ideograph is not a whole number of columns in a Latin monospace
        // font, so a blank over it would slide the rest of the line sideways out
        // from under its own colour. The character goes into every layer
        // instead, and the line is budgeted as if it were two columns wide.
        let src = "// 中文注释 let x = \"字符串\"";
        for kind in 0..KIND_COUNT {
            let s = layer(src, Lang::Rust, 0.0, 0.0, Kind::from_int(kind));
            for (i, c) in src.chars().enumerate() {
                if !c.is_ascii() {
                    assert_eq!(s.chars().nth(i), Some(c), "{kind:?} blanked a wide char");
                }
            }
        }
        // ASCII next to it still blanks normally.
        assert_eq!(
            layer("let x 注释", Lang::Rust, 0.0, 0.0, Kind::Keyword),
            "let \u{a0} 注释"
        );
        // Budgeted wide, the line still fits the row it was given: 6 columns
        // hold three of them at 2 columns each, and the fourth starts a new one.
        let wrapped = layer("中文中文", Lang::Rust, 6.0, 1.0, Kind::Plain);
        assert_eq!(wrapped, "中文中\n文");
    }

    #[test]
    fn blank_columns_are_not_breaks_and_real_ones_are() {
        // The blanking must not invent a place to break inside a word, or the
        // five layers would stop agreeing with each other.
        let src = "identifier another";
        let p = layer(src, Lang::Rust, 12.0, 1.0, Kind::Number);
        assert_eq!(p.matches('\n').count(), 1);
        assert_eq!(p.chars().count(), src.chars().count());
    }
}
