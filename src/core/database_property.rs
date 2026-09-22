// The property system (SPEC §三十九 "属性类型"): what each of the fourteen kinds
// means on the way in, on the way out, and on the way into an `ORDER BY`.
//
// Three things live here and nothing else does:
//
//   1. **The option list** (ADR-0061/ADR-0071). A select / status /
//      multi-select column's `config` is the one JSON document the database
//      layer keeps, and it holds options *with their own ids*, because a cell
//      stores an id and never a label: renaming an option is then one edit
//      that touches no value. That is ADR-0026's rule ("store the id, not the
//      text") applied to a column's own settings.
//   2. **The input and paint rules** (ADR-0069/ADR-0071): a cell's value is
//      parsed from what a user typed and painted back through the column's
//      settings. Every kind's tolerance is code here rather than prose
//      somewhere else — a number that is not a number is refused, a URL that
//      is not a URL is stored anyway.
//   3. **The JSON reader.** This project has no serde (ADR-0001: one process,
//      one runtime), and ADR-0064's view document is JSON too, so the crate
//      has exactly one reader and it lives here rather than in two places. It
//      has a depth limit, because the document we are handed may have been
//      written by a build whose writer we do not trust to be finite.
//
// Pure, like every other `core` module: no SQL, no Slint, no clock. The two
// derived time kinds are *not* read here — their source is `db_records` and the
// store projects them (ADR-0068) — this module only knows how to paint one once
// it has it.

use super::database::{CellValue, Property, PropertyKind};

/// Anything a cell editor can get wrong: a number that is not a number, a date
/// that is not the stored shape, an option name the column does not have, or a
/// write aimed at a column that stores nothing. The caller shows one line, so
/// the strings are written to be read (the same contract as
/// `services::attachment_store::AttachmentError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(String);

impl InputError {
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InputError {}

fn refused(msg: impl Into<String>) -> InputError {
    InputError(msg.into())
}

// ─── the option list (ADR-0061) ─────────────────────────────────────────────

/// What `color` holds when an option has no colour. A colour *name* is the
/// document's word and never a paint: what "green" looks like is the UI's
/// business, which is why `core` keeps it as the string it read (hard rule 3 —
/// the core does not know colours).
pub const OPTION_COLOR_NONE: &str = "";

/// One selectable option. Ids are per column and never global: a value row
/// stores the option's id in `db_values.text` (ADR-0062), so two columns may
/// each have an option 7 and nothing is confused by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyOption {
    pub id: OptionId,
    pub name: String,
    pub color: String,
}

/// An option's own id, newtyped so a cell's `text` (which *is* this id, spelled
/// as digits) cannot be passed where the id is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OptionId(pub u64);

impl OptionId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for OptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The `config` document of a column that has options: `{"options":[…]}`.
///
/// Reading is **folding, never failing** (the rule ADR-0061 sets for an unknown
/// kind): a document that does not parse, and an option without an id, are
/// dropped rather than allowed to fail a library open. What survives a broken
/// document is the *values* — they are ids in `db_values` — and a cell whose
/// option is not in the list paints its own id instead of disappearing
/// (ADR-0069).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyOptions {
    options: Vec<PropertyOption>,
}

impl PropertyOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// The column's list as the document holds it. Unknown keys are ignored; a
    /// repeated id keeps its first meaning, because "one id, one name" is the
    /// invariant the id-in-the-value rule depends on.
    pub fn from_config(config: &str) -> Self {
        let mut options: Vec<PropertyOption> = Vec::new();
        let Ok(document) = json::Json::parse(config) else {
            return Self::new();
        };
        let Some(list) = document.get("options").and_then(json::Json::as_array) else {
            return Self::new();
        };
        for entry in list {
            let Some(id) = entry.get("id").and_then(json::Json::as_u64) else {
                continue;
            };
            if options.iter().any(|o| o.id == OptionId(id)) {
                continue;
            }
            options.push(PropertyOption {
                id: OptionId(id),
                name: entry
                    .get("name")
                    .and_then(json::Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                color: entry
                    .get("color")
                    .and_then(json::Json::as_str)
                    .unwrap_or(OPTION_COLOR_NONE)
                    .to_string(),
            });
        }
        Self { options }
    }

    /// The document, in the one shape this build writes: `id` and `name`
    /// always, `color` only when it is set. Writing the shape we read is what
    /// makes "rename an option is one edit that touches no value" true rather
    /// than almost true.
    pub fn to_config(&self) -> String {
        let entries: Vec<json::Json> = self
            .options
            .iter()
            .map(|option| {
                let mut fields = vec![
                    (
                        "id".to_string(),
                        json::Json::Number(option.id.as_u64() as f64),
                    ),
                    ("name".to_string(), json::Json::Text(option.name.clone())),
                ];
                if !option.color.is_empty() {
                    fields.push(("color".to_string(), json::Json::Text(option.color.clone())));
                }
                json::Json::Object(fields)
            })
            .collect();
        json::Json::Object(vec![("options".to_string(), json::Json::Array(entries))]).to_text()
    }

    pub fn iter(&self) -> impl Iterator<Item = &PropertyOption> {
        self.options.iter()
    }

    pub fn ids(&self) -> impl Iterator<Item = OptionId> + '_ {
        self.options.iter().map(|o| o.id)
    }

    pub fn len(&self) -> usize {
        self.options.len()
    }

    pub fn is_empty(&self) -> bool {
        self.options.is_empty()
    }

    pub fn get(&self, id: OptionId) -> Option<&PropertyOption> {
        self.options.iter().find(|o| o.id == id)
    }

    /// The option a user meant by typing `name`: exact match first, then a
    /// case-insensitive one, because "Done" and "done" are one option to a
    /// writer and two only to a machine.
    pub fn named(&self, name: &str) -> Option<&PropertyOption> {
        self.options.iter().find(|o| o.name == name).or_else(|| {
            self.options
                .iter()
                .find(|o| o.name.eq_ignore_ascii_case(name))
        })
    }

    /// The option called `name`, adding it when the column does not have one.
    /// This is the shape of "type a new option into a cell": the *column* grows
    /// an option and the *cell* points at it — two changes in one batch — and
    /// the id comes from here so the caller never invents one.
    pub fn option_named(&mut self, name: &str) -> OptionId {
        if let Some(existing) = self.named(name) {
            return existing.id;
        }
        let id = OptionId(self.options.iter().map(|o| o.id.0).max().unwrap_or(0) + 1);
        self.options.push(PropertyOption {
            id,
            name: name.to_string(),
            color: OPTION_COLOR_NONE.to_string(),
        });
        id
    }

    /// Rename an option in place. Values are untouched — they store the id —
    /// and `false` means the id is not in the list, which is a caller mistake
    /// rather than a document to rewrite.
    pub fn rename(&mut self, id: OptionId, name: &str) -> bool {
        match self.options.iter_mut().find(|o| o.id == id) {
            Some(option) => {
                option.name = name.to_string();
                true
            }
            None => false,
        }
    }

    /// Remove an option from the list. Values that pointed at it keep the id
    /// (nothing cascades into `db_values`) and paint it, which is ADR-0069's
    /// unknown-option rule — visibly a missing option rather than a silent
    /// blank cell.
    pub fn remove(&mut self, id: OptionId) -> bool {
        let before = self.options.len();
        self.options.retain(|o| o.id != id);
        self.options.len() != before
    }

    /// Give an option a colour name, if the id is in the list.
    pub fn set_color(&mut self, id: OptionId, color: &str) -> bool {
        match self.options.iter_mut().find(|o| o.id == id) {
            Some(option) => {
                option.color = color.to_string();
                true
            }
            None => false,
        }
    }
}

// ─── the settings a value is painted through (ADR-0061, ADR-0069) ───────────

/// How a number is written out. `plain` is the number as stored; the other two
/// are the ones a note actually uses, and the point of keeping them in `config`
/// rather than in the cell is that changing a format touches no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumberFormat {
    #[default]
    Plain,
    /// Whole numbers, rounded half away from zero.
    Integer,
    /// Stored as a fraction (`0.25`), shown as a percentage (`25%`).
    Percent,
}

impl NumberFormat {
    /// The document's word, or `plain` — an unknown format is not an error, so
    /// a column written by a later build still paints a number.
    pub fn from_config(config: &str) -> Self {
        match config_string(config, "format").as_deref() {
            Some("integer") => NumberFormat::Integer,
            Some("percent") => NumberFormat::Percent,
            _ => NumberFormat::Plain,
        }
    }

    pub fn paint(self, num: f64) -> String {
        match self {
            NumberFormat::Plain => plain_number(num),
            NumberFormat::Integer => format!("{}", num.round()),
            // `0.1 * 100.0` is 10.000000000000002 in binary floating point, so
            // the product is rounded to six decimals before it is printed: a
            // percentage is a label, and a label may not show the FPU's
            // leftovers.
            NumberFormat::Percent => {
                let percent = (num * 100.0 * 1e6).round() / 1e6;
                format!("{}%", plain_number(percent))
            }
        }
    }
}

/// How much of a stored date is shown. The stored text is the truth either way
/// — this decides only how much of it a cell prints (ADR-0062 keeps the value
/// fixed-width, which is what makes the sort work).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateFormat {
    /// `YYYY-MM-DD`, the first ten bytes of the stored text.
    #[default]
    Date,
    /// The whole stored text, which is ten bytes for a date and sixteen for a
    /// stamp. Never invents a time: a date-only value in a `datetime` column
    /// still prints ten bytes.
    DateTime,
}

impl DateFormat {
    /// The document's word, or the kind's own default: a `date` property shows
    /// a day unless told otherwise, while `created time` / `last edited time`
    /// show the minute — a stamp with no time would make two rows written the
    /// same day look identical.
    pub fn for_kind(kind: PropertyKind, config: &str) -> Self {
        match (config_string(config, "format").as_deref(), kind) {
            (Some("date"), _) => DateFormat::Date,
            (Some("datetime"), _) => DateFormat::DateTime,
            (_, PropertyKind::CreatedTime | PropertyKind::LastEditedTime) => DateFormat::DateTime,
            _ => DateFormat::Date,
        }
    }

    pub fn paint(self, stored: &str) -> String {
        match self {
            DateFormat::Date => stored.get(..10).unwrap_or(stored).to_string(),
            DateFormat::DateTime => stored.to_string(),
        }
    }
}

/// One `"key": "value"` out of a config document, for the settings that are a
/// single word. Anything else (a missing document, a number, an array) is
/// `None`, which every reader turns into its own default.
fn config_string(config: &str, key: &str) -> Option<String> {
    json::Json::parse(config)
        .ok()?
        .get(key)
        .and_then(json::Json::as_str)
        .map(str::to_string)
}

/// How a number is written with no settings at all: Rust's own `Display`, so a
/// whole number does not paint a trailing `.0`. `CellValue::display` uses the
/// same thing, which is why a cell and a value never disagree about one.
fn plain_number(num: f64) -> String {
    format!("{num}")
}

// ─── the input path (ADR-0069) ──────────────────────────────────────────────

/// What a cell editor typed, as a stored value. One rule covers every kind:
/// **no characters means no value** (`CellValue::Empty`, which is the absence
/// of a row) — the input path never stores a blank. `Text("")` stays reachable
/// for a writer that says so on purpose, and the two paint the same.
///
/// | kind | accepted | refused |
/// |------|----------|---------|
/// | title / text | anything, verbatim, any length | nothing |
/// | number | a finite decimal, `e` notation, either sign | `inf`, `NaN`, prose |
/// | date | `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM` | any other shape |
/// | checkbox | yes/no/on/off/true/false/x/unchecked | anything else |
/// | url / email / phone | **anything, verbatim** | nothing |
/// | select / status | a name the column has an option for | a name it does not |
/// | multi-select / files / relation | [`parse_many`] | — |
/// | computed / derived | nothing: these columns store no cell | every write |
pub fn parse_one(kind: PropertyKind, config: &str, input: &str) -> Result<CellValue, InputError> {
    // The columns that store nothing refuse before anything else: "clear it"
    // is not a write either, because the derivation would put it straight back.
    if kind.is_computed() || kind.is_derived() {
        return Err(refused(format!(
            "{} stores no cell (ADR-0039: it is derived)",
            kind.as_str()
        )));
    }
    // No characters means no value. Text keeps a run of spaces, because a space
    // can be the content there; every other kind trims before it decides,
    // because " 2 " is a number a user typed.
    let blank = if matches!(kind, PropertyKind::Title | PropertyKind::Text) {
        input.is_empty()
    } else {
        input.trim().is_empty()
    };
    if blank {
        return Ok(CellValue::Empty);
    }
    match kind {
        PropertyKind::Title | PropertyKind::Text => Ok(CellValue::Text(input.to_string())),
        PropertyKind::Number => {
            let text = input.trim();
            let num = text
                .parse::<f64>()
                .map_err(|_| refused(format!("\"{text}\" is not a number")))?;
            if !num.is_finite() {
                // `f64::from_str` accepts "inf", "-inf" and "NaN", and a NaN in
                // the `num` column is a row that sorts by bit pattern and
                // compares as nothing. Refused by name.
                return Err(refused(format!("\"{text}\" is not a finite number")));
            }
            Ok(CellValue::Number(num))
        }
        PropertyKind::Date => match iso_date(input) {
            Some(stored) => Ok(CellValue::Text(stored)),
            None => Err(refused(format!(
                "\"{}\" is not a date of the stored shape",
                input.trim()
            ))),
        },
        PropertyKind::Checkbox => match input.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "x" | "checked" => Ok(CellValue::Flag(true)),
            "false" | "no" | "off" | "unchecked" => Ok(CellValue::Flag(false)),
            other => Err(refused(format!("\"{other}\" is not a checkbox"))),
        },
        // The three §三十九 kinds that are a string and only a string: nothing
        // is trimmed, normalised or case-folded. A link the user typed is the
        // link they typed, and `looks_valid` is a hint for the cell editor, not
        // a gate (ADR-0069).
        PropertyKind::Url | PropertyKind::Email | PropertyKind::Phone => {
            Ok(CellValue::Text(input.to_string()))
        }
        PropertyKind::Select | PropertyKind::Status => {
            let options = PropertyOptions::from_config(config);
            match options.named(input.trim()) {
                Some(option) => Ok(CellValue::Text(option.id.to_string())),
                // Not auto-created here: an option list is a second change in
                // the same batch, and inventing an id in the cell writer would
                // leave a value nothing can name. `PropertyOptions::option_named`
                // is the get-or-add the caller wants.
                None => Err(refused(format!(
                    "column \"{}\" has no option \"{}\"",
                    kind.as_str(),
                    input.trim()
                ))),
            }
        }
        PropertyKind::MultiSelect | PropertyKind::Files | PropertyKind::Relation => {
            Err(refused(format!(
                "{} takes a list, not one value",
                kind.as_str()
            )))
        }
        // Handled above, and listed so that adding a kind cannot slip past this
        // match without a decision.
        PropertyKind::CreatedTime
        | PropertyKind::LastEditedTime
        | PropertyKind::Formula
        | PropertyKind::Rollup => Err(refused(format!(
            "{} stores no cell (ADR-0039: it is derived)",
            kind.as_str()
        ))),
    }
}

/// The list kinds' input: multi-select takes **option names** (mapped to the
/// ids the cells store, unknown names refused for the same reason as above),
/// and files takes **attachment ids** — the attachment channel has already
/// written the bytes, and ADR-0029/ADR-0030's rows are what a files cell points
/// at, never a second copy of them. Whether the row still exists is a question
/// only the store can answer, so this checks the *shape*.
///
/// An empty list is the absence of a value, like an empty input.
pub fn parse_many(
    kind: PropertyKind,
    config: &str,
    inputs: &[String],
) -> Result<CellValue, InputError> {
    if inputs.is_empty() {
        return Ok(CellValue::Empty);
    }
    match kind {
        PropertyKind::MultiSelect => {
            let options = PropertyOptions::from_config(config);
            let mut ids = Vec::with_capacity(inputs.len());
            for name in inputs {
                match options.named(name.trim()) {
                    Some(option) => ids.push(option.id.to_string()),
                    None => {
                        return Err(refused(format!(
                            "column \"multi_select\" has no option \"{}\"",
                            name.trim()
                        )))
                    }
                }
            }
            Ok(CellValue::Items(ids))
        }
        PropertyKind::Files => {
            let mut ids = Vec::with_capacity(inputs.len());
            for id in inputs {
                let text = id.trim();
                // Canonical digits only: the id a cell stores is the id the
                // attachment row has, spelled the one way `to_string` spells it.
                // "007" and "0" and "+3" are refused, which is also what makes
                // the painted fallback for a dead id a number a user can read.
                let canonical = text
                    .parse::<u64>()
                    .map(|id| id > 0 && id.to_string() == text)
                    .unwrap_or(false);
                if !canonical {
                    return Err(refused(format!("\"{text}\" is not an attachment id")));
                }
                ids.push(text.to_string());
            }
            Ok(CellValue::Items(ids))
        }
        // A relation's list is **record ids**, in the same canonical-digits
        // shape a files cell's attachment ids take (ADR-0088): the id a cell
        // stores is the id the record has, spelled the one way `to_string`
        // spells it. That is what makes the painted fallback for a dead target
        // a number a reader can compare against a row, rather than a string a
        // writer could have spelled two ways.
        PropertyKind::Relation => {
            let mut ids = Vec::with_capacity(inputs.len());
            for id in inputs {
                let text = id.trim();
                let canonical = text
                    .parse::<u64>()
                    .map(|id| id > 0 && id.to_string() == text)
                    .unwrap_or(false);
                if !canonical {
                    return Err(refused(format!("\"{text}\" is not a record id")));
                }
                ids.push(text.to_string());
            }
            Ok(CellValue::Items(ids))
        }
        other => Err(refused(format!(
            "{} takes one value, not a list",
            other.as_str()
        ))),
    }
}

/// The stored shape of a date, or `None` when the text is not it. **The shape
/// is the invariant, not the calendar**: `2026-02-30` is stored as typed,
/// because this is a notebook and not a scheduler, while `2026-9-2` is refused,
/// because a date column whose bytes are not fixed-width sorts wrong — which is
/// the one thing ADR-0062 buys with the ISO form. (`YYYY-MM-DD` and
/// `YYYY-MM-DDTHH:MM`, both zero-padded, both ten or sixteen bytes of ASCII.)
pub fn iso_date(text: &str) -> Option<String> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let shaped = match bytes.len() {
        10 => bytes[4] == b'-' && bytes[7] == b'-' && digits_except(bytes, &[4, 7]),
        16 => {
            bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes[10] == b'T'
                && bytes[13] == b':'
                && digits_except(bytes, &[4, 7, 10, 13])
        }
        _ => false,
    };
    if !shaped {
        return None;
    }
    let pair = |at: usize| (bytes[at] - b'0') as u32 * 10 + (bytes[at + 1] - b'0') as u32;
    let (month, day) = (pair(5), pair(8));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if bytes.len() == 16 && (pair(11) > 23 || pair(14) > 59) {
        return None;
    }
    Some(text.to_string())
}

/// Every byte is an ASCII digit except the separators named. `bytes` has been
/// length-checked by the caller, so the indexing is in bounds.
fn digits_except(bytes: &[u8], separators: &[usize]) -> bool {
    bytes
        .iter()
        .enumerate()
        .all(|(at, byte)| separators.contains(&at) || byte.is_ascii_digit())
}

/// A hint, never a gate (ADR-0069): whether `text` looks like the kind it is
/// meant to be. A cell editor may dot a cell with it; nothing here refuses a
/// write, and nothing rewrites what the user typed. `true` for every kind that
/// has no opinion, so a caller can ask unguarded.
pub fn looks_valid(kind: PropertyKind, text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    match kind {
        PropertyKind::Url => {
            !text.chars().any(char::is_whitespace)
                && (text.starts_with("www.") || scheme_of(text).is_some())
        }
        PropertyKind::Email => {
            let mut parts = text.split('@');
            let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next())
            else {
                return false;
            };
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !text.chars().any(char::is_whitespace)
        }
        PropertyKind::Phone => {
            text.chars().filter(|c| c.is_ascii_digit()).count() >= 5
                && text
                    .chars()
                    .all(|c| c.is_ascii_digit() || " +-()./".contains(c))
        }
        _ => true,
    }
}

/// The scheme of a URL, when it has one. Deliberately a lexical question
/// (`[a-z][a-z0-9+.-]*` then `://`) and not a URL parser: ADR-0001's one
/// process and no web runtime, and nothing here opens a link anyway.
fn scheme_of(text: &str) -> Option<&str> {
    let (scheme, _) = text.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    chars
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        .then_some(scheme)
}

// ─── the paint path (ADR-0069) ──────────────────────────────────────────────

/// The names of attachments, for the one kind whose painted form is not in its
/// value: a `files` cell stores attachment ids (ADR-0062) and a user reads file
/// names. `core` cannot query `attachments` (hard rule 1), so the caller that
/// can — the store, already inside the read — hands the answer over. `()` is
/// the honest "nobody looked", and paints ids.
pub trait AttachmentNames {
    fn attachment_name(&self, id: &str) -> Option<String>;
}

impl AttachmentNames for () {
    fn attachment_name(&self, _id: &str) -> Option<String> {
        None
    }
}

impl AttachmentNames for std::collections::HashMap<String, String> {
    fn attachment_name(&self, id: &str) -> Option<String> {
        self.get(id).cloned()
    }
}

/// One cell as a user reads it, through the column's own settings.
///
/// Everything a kind needs beyond its value is here and nowhere else: an option
/// id becomes the option's **name**, a number goes through its `NumberFormat`,
/// a date through its `DateFormat`, a file id through the attachment names, a
/// multi-select joins its names. A kind with no settings falls through to
/// `CellValue::display`, which is the value's own form — so the two can never
/// disagree about a number.
///
/// Two folds, both ADR-0069: an option id the column does not list paints
/// **itself** (a missing option is visible, and the value is still there), and
/// a file id whose attachment row is gone paints **itself** for the same
/// reason.
pub fn paint(
    kind: PropertyKind,
    config: &str,
    value: &CellValue,
    names: &dyn AttachmentNames,
) -> String {
    match (kind, value) {
        (PropertyKind::Select | PropertyKind::Status, CellValue::Text(id)) => {
            let options = PropertyOptions::from_config(config);
            match id
                .parse::<u64>()
                .ok()
                .and_then(|id| options.get(OptionId(id)))
            {
                Some(option) => option.name.clone(),
                None => id.clone(),
            }
        }
        (PropertyKind::MultiSelect, CellValue::Items(ids)) => {
            let options = PropertyOptions::from_config(config);
            paint_list(ids, |id| {
                id.parse::<u64>()
                    .ok()
                    .and_then(|id| options.get(OptionId(id)))
                    .map(|option| option.name.clone())
            })
        }
        (PropertyKind::Files, CellValue::Items(ids)) => {
            paint_list(ids, |id| names.attachment_name(id))
        }
        (PropertyKind::Number, CellValue::Number(num)) => {
            NumberFormat::from_config(config).paint(*num)
        }
        // A relation cell holds *ids*, and an id is not a name. Turning them
        // into the live titles is the projection's job
        // (`core::database_relation::paint_targets`, which needs a store this
        // function deliberately has none of), and in practice the projection
        // has already run by the time a cell is drawn. This arm is what a
        // caller reading `RowView` directly sees, and the ids are more honest
        // than a blank: a blank would read as "this row relates to nothing",
        // which is a different fact from "these are the targets, unnamed".
        (PropertyKind::Relation, CellValue::Items(ids)) => ids.join(", "),
        (
            PropertyKind::Date | PropertyKind::CreatedTime | PropertyKind::LastEditedTime,
            CellValue::Text(stored),
        ) => DateFormat::for_kind(kind, config).paint(stored),
        _ => value.display(),
    }
}

/// A list cell's painted form: each item through `name_of`, or itself when
/// nothing can name it.
fn paint_list(ids: &[String], name_of: impl Fn(&str) -> Option<String>) -> String {
    ids.iter()
        .map(|id| name_of(id).unwrap_or_else(|| id.clone()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A column's own accessors, so a caller holding a `Property` does not have to
/// remember the argument order of four free functions — and so the kind and the
/// config that belong together are read from the same place.
impl Property {
    /// The column's option list, read out of its `config` (empty for a kind
    /// that has no options).
    pub fn options(&self) -> PropertyOptions {
        PropertyOptions::from_config(&self.config)
    }

    /// One typed cell, as the store will hold it (ADR-0069's tolerance table).
    pub fn parse(&self, input: &str) -> Result<CellValue, InputError> {
        parse_one(self.kind, &self.config, input)
    }

    /// One list-typed cell, from the items a user picked.
    pub fn parse_many(&self, inputs: &[String]) -> Result<CellValue, InputError> {
        parse_many(self.kind, &self.config, inputs)
    }

    /// What a user reads in this column's cell.
    pub fn paint(&self, value: &CellValue, names: &dyn AttachmentNames) -> String {
        paint(self.kind, &self.config, value, names)
    }
}

// ─── the one JSON reader ────────────────────────────────────────────────────

/// A JSON document, and the only reader of one in this crate (ADR-0061's option
/// list today, ADR-0064's view document in D4).
///
/// It is a *reader*, and the writer below exists for one reason: a column's
/// option list has to be able to write itself back in the shape it was read in.
/// There is no serde and no derive — the point is a document whose shape this
/// project controls, and a dependency for two documents would be a bigger
/// surface than the hundred lines it saves.
pub mod json {

    /// Nesting limit. A config document is written by this app or by another
    /// build of it, and "another build" is not a reason to let a hostile file
    /// recurse until the stack ends. Sixteen is far past any document either
    /// reader describes (an option list is depth 3).
    const MAX_DEPTH: usize = 16;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Json {
        Null,
        Bool(bool),
        Number(f64),
        Text(String),
        Array(Vec<Json>),
        Object(Vec<(String, Json)>),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct JsonError(String);

    impl std::fmt::Display for JsonError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for JsonError {}

    impl Json {
        /// A document, or the byte at which it stopped being one. Trailing
        /// content after the value is an error: half a document read as a whole
        /// one is how a config silently loses its options.
        pub fn parse(text: &str) -> Result<Json, JsonError> {
            let mut parser = Parser {
                bytes: text.as_bytes(),
                at: 0,
                depth: 0,
            };
            parser.space();
            let value = parser.value()?;
            parser.space();
            if parser.at != parser.bytes.len() {
                return Err(parser.error("trailing characters"));
            }
            Ok(value)
        }

        pub fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        pub fn as_str(&self) -> Option<&str> {
            match self {
                Json::Text(text) => Some(text),
                _ => None,
            }
        }

        pub fn as_f64(&self) -> Option<f64> {
            match self {
                Json::Number(num) => Some(*num),
                _ => None,
            }
        }

        /// A number that is a whole, non-negative, finite one — the shape an id
        /// or a count has. `7.5`, `-1` and `1e400` are none of those things.
        pub fn as_u64(&self) -> Option<u64> {
            match self {
                Json::Number(num)
                    if num.is_finite() && *num >= 0.0 && num.fract() == 0.0 && *num < 1e18 =>
                {
                    Some(*num as u64)
                }
                _ => None,
            }
        }

        pub fn as_array(&self) -> Option<&[Json]> {
            match self {
                Json::Array(items) => Some(items),
                _ => None,
            }
        }

        /// The compact form, in the order the fields were built. Integral
        /// numbers print without a decimal point (`7`, not `7.0`), which is what
        /// keeps an option id an id when the list writes itself back.
        pub fn to_text(&self) -> String {
            let mut out = String::new();
            self.write(&mut out);
            out
        }

        fn write(&self, out: &mut String) {
            match self {
                Json::Null => out.push_str("null"),
                Json::Bool(true) => out.push_str("true"),
                Json::Bool(false) => out.push_str("false"),
                Json::Number(num) => {
                    if num.is_finite() && num.fract() == 0.0 && num.abs() < 1e15 {
                        out.push_str(&format!("{}", *num as i64));
                    } else {
                        out.push_str(&format!("{num}"));
                    }
                }
                Json::Text(text) => write_string(text, out),
                Json::Array(items) => {
                    out.push('[');
                    for (at, item) in items.iter().enumerate() {
                        if at > 0 {
                            out.push(',');
                        }
                        item.write(out);
                    }
                    out.push(']');
                }
                Json::Object(fields) => {
                    out.push('{');
                    for (at, (key, value)) in fields.iter().enumerate() {
                        if at > 0 {
                            out.push(',');
                        }
                        write_string(key, out);
                        out.push(':');
                        value.write(out);
                    }
                    out.push('}');
                }
            }
        }
    }

    fn write_string(text: &str, out: &mut String) {
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }

    /// A byte cursor. Every method leaves `at` on the byte *after* what it
    /// consumed, so the string loop below can step past each escape by
    /// consuming it whole rather than by guessing how long it was.
    struct Parser<'a> {
        bytes: &'a [u8],
        at: usize,
        depth: usize,
    }

    impl Parser<'_> {
        fn error(&self, what: &str) -> JsonError {
            JsonError(format!("{what} at byte {}", self.at))
        }

        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.at).copied()
        }

        fn space(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.at += 1;
            }
        }

        fn expect(&mut self, byte: u8) -> Result<(), JsonError> {
            if self.peek() == Some(byte) {
                self.at += 1;
                Ok(())
            } else {
                Err(self.error(&format!("expected {}", byte as char)))
            }
        }

        fn literal(&mut self, word: &str) -> bool {
            if self.bytes[self.at..].starts_with(word.as_bytes()) {
                self.at += word.len();
                true
            } else {
                false
            }
        }

        fn value(&mut self) -> Result<Json, JsonError> {
            if self.depth >= MAX_DEPTH {
                return Err(self.error("too deeply nested"));
            }
            match self.peek() {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => self.string().map(Json::Text),
                Some(b't') if self.literal("true") => Ok(Json::Bool(true)),
                Some(b'f') if self.literal("false") => Ok(Json::Bool(false)),
                Some(b'n') if self.literal("null") => Ok(Json::Null),
                Some(_) => self.number(),
                None => Err(self.error("expected a value")),
            }
        }

        fn object(&mut self) -> Result<Json, JsonError> {
            self.expect(b'{')?;
            self.depth += 1;
            let mut fields: Vec<(String, Json)> = Vec::new();
            self.space();
            if self.peek() == Some(b'}') {
                self.at += 1;
                self.depth -= 1;
                return Ok(Json::Object(fields));
            }
            loop {
                self.space();
                let key = self.string()?;
                self.space();
                self.expect(b':')?;
                self.space();
                let value = self.value()?;
                // A repeated key keeps its last meaning, which is what every
                // other JSON reader does; `get` above then finds one answer.
                fields.retain(|(existing, _)| existing != &key);
                fields.push((key, value));
                self.space();
                match self.peek() {
                    Some(b',') => self.at += 1,
                    Some(b'}') => {
                        self.at += 1;
                        self.depth -= 1;
                        return Ok(Json::Object(fields));
                    }
                    _ => return Err(self.error("expected , or }")),
                }
            }
        }

        fn array(&mut self) -> Result<Json, JsonError> {
            self.expect(b'[')?;
            self.depth += 1;
            let mut items = Vec::new();
            self.space();
            if self.peek() == Some(b']') {
                self.at += 1;
                self.depth -= 1;
                return Ok(Json::Array(items));
            }
            loop {
                self.space();
                items.push(self.value()?);
                self.space();
                match self.peek() {
                    Some(b',') => self.at += 1,
                    Some(b']') => {
                        self.at += 1;
                        self.depth -= 1;
                        return Ok(Json::Array(items));
                    }
                    _ => return Err(self.error("expected , or ]")),
                }
            }
        }

        fn string(&mut self) -> Result<String, JsonError> {
            self.expect(b'"')?;
            let mut out = String::new();
            loop {
                match self.peek() {
                    None => return Err(self.error("unterminated string")),
                    Some(b'"') => {
                        self.at += 1;
                        return Ok(out);
                    }
                    Some(b'\\') => {
                        self.at += 1;
                        let c = match self.peek() {
                            Some(b'"') => {
                                self.at += 1;
                                '"'
                            }
                            Some(b'\\') => {
                                self.at += 1;
                                '\\'
                            }
                            Some(b'/') => {
                                self.at += 1;
                                '/'
                            }
                            Some(b'b') => {
                                self.at += 1;
                                '\u{8}'
                            }
                            Some(b'f') => {
                                self.at += 1;
                                '\u{c}'
                            }
                            Some(b'n') => {
                                self.at += 1;
                                '\n'
                            }
                            Some(b'r') => {
                                self.at += 1;
                                '\r'
                            }
                            Some(b't') => {
                                self.at += 1;
                                '\t'
                            }
                            Some(b'u') => {
                                self.at += 1;
                                self.unicode_escape()?
                            }
                            _ => return Err(self.error("bad escape")),
                        };
                        out.push(c);
                    }
                    Some(byte) if byte < 0x20 => {
                        return Err(self.error("raw control character in a string"))
                    }
                    Some(_) => {
                        // Copy the whole UTF-8 character: the document is UTF-8,
                        // and a byte-wise copy would split a multi-byte
                        // character on the way through.
                        let rest = std::str::from_utf8(&self.bytes[self.at..])
                            .map_err(|_| self.error("not UTF-8"))?;
                        let c = rest.chars().next().ok_or_else(|| self.error("empty"))?;
                        out.push(c);
                        self.at += c.len_utf8();
                    }
                }
            }
        }

        /// `\uXXXX`, and `\uXXXX\uYYYY` when the first is a high surrogate —
        /// which is how JSON spells a character outside the BMP. A lone
        /// surrogate becomes the replacement character: it is not a `char` in
        /// Rust, and a document that is malformed here is still a document whose
        /// other fields are worth reading.
        fn unicode_escape(&mut self) -> Result<char, JsonError> {
            let high = self.hex4()?;
            if (0xd800..0xdc00).contains(&high)
                && self.peek() == Some(b'\\')
                && self.bytes.get(self.at + 1) == Some(&b'u')
            {
                self.at += 2;
                let low = self.hex4()?;
                if (0xdc00..0xe000).contains(&low) {
                    let code = 0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00);
                    return Ok(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                return Ok('\u{fffd}');
            }
            Ok(char::from_u32(high).unwrap_or('\u{fffd}'))
        }

        fn hex4(&mut self) -> Result<u32, JsonError> {
            let mut code = 0u32;
            for _ in 0..4 {
                let byte = self.peek().ok_or_else(|| self.error("short \\u"))?;
                let digit = (byte as char)
                    .to_digit(16)
                    .ok_or_else(|| self.error("bad \\u digit"))?;
                code = code * 16 + digit;
                self.at += 1;
            }
            Ok(code)
        }

        fn number(&mut self) -> Result<Json, JsonError> {
            let start = self.at;
            if self.peek() == Some(b'-' | b'+') {
                self.at += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')) {
                self.at += 1;
            }
            let text = std::str::from_utf8(&self.bytes[start..self.at])
                .map_err(|_| self.error("not UTF-8"))?;
            text.parse::<f64>()
                .map(Json::Number)
                .map_err(|_| self.error("not a number"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::json::Json;
    use super::*;
    use crate::core::database::{DatabaseId, PropertyId};
    use crate::core::types::OrderKey;
    use std::collections::HashMap;

    fn column(kind: PropertyKind, config: &str) -> Property {
        Property {
            id: PropertyId(1),
            db: DatabaseId(1),
            name: "P1".into(),
            kind,
            config: config.into(),
            ord: OrderKey::FIRST,
        }
    }

    const STATUS_CONFIG: &str =
        r#"{"options":[{"id":7,"name":"Done","color":"green"},{"id":2,"name":"Doing"}]}"#;

    // ─── the JSON reader ───────────────────────────────────────────────────

    #[test]
    fn the_json_reader_reads_what_the_writer_writes() {
        let document = Json::Object(vec![
            (
                "options".into(),
                Json::Array(vec![Json::Object(vec![
                    ("id".into(), Json::Number(7.0)),
                    ("name".into(), Json::Text("Done".into())),
                ])]),
            ),
            ("v".into(), Json::Number(1.0)),
        ]);
        let text = document.to_text();
        assert_eq!(
            text, r#"{"options":[{"id":7,"name":"Done"}],"v":1}"#,
            "an integral number prints without a decimal point"
        );
        assert_eq!(Json::parse(&text).unwrap(), document, "and reads back equal");
    }

    #[test]
    fn the_json_reader_handles_escapes_and_unicode() {
        let parsed = Json::parse(r#"{"name":"a\"b\\c\nd","emoji":"\ud83d\ude00","plain":"héllo"}"#)
            .unwrap();
        assert_eq!(parsed.get("name").unwrap().as_str(), Some("a\"b\\c\nd"));
        assert_eq!(parsed.get("emoji").unwrap().as_str(), Some("😀"));
        assert_eq!(parsed.get("plain").unwrap().as_str(), Some("héllo"));
        // A lone surrogate is not a `char`: it folds rather than failing the
        // whole document, because the *other* fields are still worth reading.
        assert_eq!(Json::parse(r#""\ud800""#).unwrap().as_str(), Some("\u{fffd}"));
        // And the writer spells the escapes back.
        assert_eq!(Json::Text("a\"b\\c\nd".into()).to_text(), r#""a\"b\\c\nd""#);
        assert_eq!(Json::Text("\u{1}".into()).to_text(), r#""\u0001""#);
    }

    #[test]
    fn the_json_reader_refuses_what_is_not_one_document() {
        for bad in [
            "",
            "{",
            "{\"a\":}",
            "{\"a\":1,}",
            "[1,2",
            "nope",
            "{} {}",
            "1 2",
            "\"unterminated",
            "{\"a\" 1}",
            "{\"a\":1}}",
        ] {
            assert!(Json::parse(bad).is_err(), "{bad:?} parsed");
        }
        // Depth is capped, so a document that is a mile of brackets cannot end
        // the process.
        let deep = format!("{}0{}", "[".repeat(64), "]".repeat(64));
        assert!(Json::parse(&deep).is_err(), "depth limit");
        // Numbers: the id reader wants whole, non-negative, finite ones.
        assert_eq!(Json::Number(7.0).as_u64(), Some(7));
        assert_eq!(Json::Number(7.5).as_u64(), None);
        assert_eq!(Json::Number(-1.0).as_u64(), None);
        assert_eq!(Json::Number(f64::INFINITY).as_u64(), None);
        assert_eq!(Json::parse("1e400").unwrap().as_u64(), None);
        assert_eq!(Json::parse("{\"a\":1.5}").unwrap().get("a").unwrap().as_f64(), Some(1.5));
    }

    // ─── the option list ───────────────────────────────────────────────────

    #[test]
    fn an_option_list_round_trips_and_renames_by_id() {
        let options = PropertyOptions::from_config(STATUS_CONFIG);
        assert_eq!(options.len(), 2);
        assert_eq!(options.get(OptionId(7)).unwrap().name, "Done");
        assert_eq!(options.get(OptionId(7)).unwrap().color, "green");
        assert_eq!(
            options.get(OptionId(2)).unwrap().color,
            OPTION_COLOR_NONE,
            "a missing colour is not an error"
        );
        assert_eq!(options.named("Doing").unwrap().id, OptionId(2));
        assert_eq!(options.named("doing").unwrap().id, OptionId(2), "case folds");
        assert_eq!(options.named("Waiting"), None);

        // The document the list writes is the document it read.
        assert_eq!(
            options.to_config(),
            r#"{"options":[{"id":7,"name":"Done","color":"green"},{"id":2,"name":"Doing"}]}"#
        );

        // Renaming is one edit that touches no value: the ids are the same
        // before and after (ADR-0061's whole reason for ids).
        let mut renamed = options.clone();
        assert!(renamed.rename(OptionId(7), "Finished"));
        assert_eq!(
            renamed.ids().collect::<Vec<_>>(),
            options.ids().collect::<Vec<_>>()
        );
        assert_eq!(renamed.get(OptionId(7)).unwrap().name, "Finished");
        assert!(!renamed.rename(OptionId(99), "Nope"), "no such option");

        // And the values the old list was painting keep pointing at option 7.
        let value = CellValue::Text("7".into());
        assert_eq!(paint(PropertyKind::Status, STATUS_CONFIG, &value, &()), "Done");
        assert_eq!(
            paint(PropertyKind::Status, &renamed.to_config(), &value, &()),
            "Finished"
        );
    }

    #[test]
    fn an_option_list_grows_by_name_without_reusing_an_id() {
        let mut options = PropertyOptions::from_config(STATUS_CONFIG);
        assert_eq!(
            options.option_named("Done"),
            OptionId(7),
            "existing, not a copy"
        );
        let fresh = options.option_named("Waiting");
        assert_eq!(fresh, OptionId(8), "one past the largest id it had");
        assert_eq!(options.len(), 3);
        assert_eq!(options.option_named("Waiting"), fresh, "get-or-add");
        // Removing takes the option out of the list and leaves the values.
        assert!(options.remove(OptionId(2)));
        assert_eq!(options.len(), 2);
        assert!(!options.remove(OptionId(2)));
        assert_eq!(
            paint(
                PropertyKind::Select,
                &options.to_config(),
                &CellValue::Text("2".into()),
                &()
            ),
            "2",
            "a value whose option is gone paints its own id"
        );
        assert!(options.set_color(fresh, "blue"));
        assert!(!options.set_color(OptionId(99), "blue"));
    }

    #[test]
    fn a_settings_document_that_is_not_one_folds_to_an_empty_list() {
        for bad in [
            "",
            "not json",
            "[]",
            "{\"options\":{}}",
            "{\"options\":[{\"name\":\"x\"}]}",
        ] {
            let options = PropertyOptions::from_config(bad);
            assert!(options.is_empty(), "{bad:?} produced options");
        }
        // A repeated id keeps its first meaning: one id, one name.
        let options = PropertyOptions::from_config(
            r#"{"options":[{"id":1,"name":"one"},{"id":1,"name":"uno"}]}"#,
        );
        assert_eq!(options.len(), 1);
        assert_eq!(options.get(OptionId(1)).unwrap().name, "one");
    }

    // ─── formats ───────────────────────────────────────────────────────────

    #[test]
    fn a_number_format_is_read_from_the_column_and_paints_a_label() {
        assert_eq!(NumberFormat::from_config(""), NumberFormat::Plain);
        assert_eq!(
            NumberFormat::from_config(r#"{"format":"percent"}"#),
            NumberFormat::Percent
        );
        assert_eq!(
            NumberFormat::from_config(r#"{"format":"integer"}"#),
            NumberFormat::Integer
        );
        assert_eq!(
            NumberFormat::from_config(r#"{"format":"wat"}"#),
            NumberFormat::Plain,
            "an unknown format is not an error"
        );
        assert_eq!(NumberFormat::Plain.paint(3.0), "3");
        assert_eq!(NumberFormat::Plain.paint(2.5), "2.5");
        assert_eq!(NumberFormat::Integer.paint(2.5), "3");
        assert_eq!(NumberFormat::Integer.paint(-2.5), "-3");
        assert_eq!(NumberFormat::Integer.paint(7.0), "7");
        assert_eq!(NumberFormat::Percent.paint(0.25), "25%");
        assert_eq!(
            NumberFormat::Percent.paint(0.1),
            "10%",
            "0.1*100 is 10.000000000000002 in binary floating point"
        );
        assert_eq!(NumberFormat::Percent.paint(-0.005), "-0.5%");
    }

    #[test]
    fn a_date_format_decides_how_much_of_the_stored_text_is_shown() {
        let day = "2026-09-22";
        let minute = "2026-09-22T14:03";
        assert_eq!(DateFormat::for_kind(PropertyKind::Date, ""), DateFormat::Date);
        assert_eq!(
            DateFormat::for_kind(PropertyKind::Date, r#"{"format":"datetime"}"#),
            DateFormat::DateTime
        );
        assert_eq!(DateFormat::Date.paint(day), day);
        assert_eq!(DateFormat::Date.paint(minute), day);
        // A stamp shows its minute by default — two rows written the same day
        // must not look identical — and never invents a time it does not have.
        assert_eq!(
            DateFormat::for_kind(PropertyKind::CreatedTime, ""),
            DateFormat::DateTime
        );
        assert_eq!(
            DateFormat::for_kind(PropertyKind::LastEditedTime, r#"{"format":"date"}"#),
            DateFormat::Date
        );
        assert_eq!(DateFormat::DateTime.paint(minute), minute);
        assert_eq!(DateFormat::DateTime.paint(day), day);
    }

    // ─── the input path ────────────────────────────────────────────────────

    #[test]
    fn an_empty_input_is_the_absence_of_a_value() {
        for kind in PropertyKind::ALL {
            if kind.is_computed() || kind.is_derived() {
                continue;
            }
            assert_eq!(
                parse_one(kind, STATUS_CONFIG, "").unwrap(),
                CellValue::Empty,
                "{kind:?}"
            );
            if !matches!(kind, PropertyKind::Text | PropertyKind::Title) {
                // For every kind but text, whitespace is nothing too.
                assert_eq!(
                    parse_one(kind, STATUS_CONFIG, "   ").unwrap(),
                    CellValue::Empty,
                    "{kind:?} with spaces"
                );
            }
        }
        // A text cell's spaces are content: "   " is three characters, not a
        // clear. Only the truly empty string clears one.
        assert_eq!(
            parse_one(PropertyKind::Text, "", "   ").unwrap(),
            CellValue::Text("   ".into())
        );
        assert_eq!(
            parse_many(PropertyKind::MultiSelect, STATUS_CONFIG, &[]).unwrap(),
            CellValue::Empty
        );
        assert_eq!(
            parse_many(PropertyKind::Files, "", &[]).unwrap(),
            CellValue::Empty
        );
    }

    #[test]
    fn a_text_cell_is_stored_exactly_as_it_was_typed() {
        let typed = "  two  spaces  and\na newline  ";
        assert_eq!(
            parse_one(PropertyKind::Text, "", typed).unwrap(),
            CellValue::Text(typed.into()),
            "no trimming: a space can be the content"
        );
        assert_eq!(
            parse_one(PropertyKind::Title, "", "  Title  ").unwrap(),
            CellValue::Text("  Title  ".into())
        );
        // A long one is a long one: there is no cap here, because the value
        // column is TEXT and the store's own row is what a cap would protect.
        let long = "x".repeat(100_000);
        assert_eq!(
            parse_one(PropertyKind::Text, "", &long).unwrap(),
            CellValue::Text(long)
        );
    }

    #[test]
    fn a_number_is_parsed_or_refused_by_name() {
        assert_eq!(
            parse_one(PropertyKind::Number, "", "2").unwrap(),
            CellValue::Number(2.0)
        );
        assert_eq!(
            parse_one(PropertyKind::Number, "", " 2.5 ").unwrap(),
            CellValue::Number(2.5)
        );
        assert_eq!(
            parse_one(PropertyKind::Number, "", "-2").unwrap(),
            CellValue::Number(-2.0),
            "a negative number is a number"
        );
        assert_eq!(
            parse_one(PropertyKind::Number, "", "0").unwrap(),
            CellValue::Number(0.0),
            "zero is a value, and it is not how empty is spelled"
        );
        assert_eq!(
            parse_one(PropertyKind::Number, "", "1e3").unwrap(),
            CellValue::Number(1000.0)
        );
        assert_eq!(
            parse_one(PropertyKind::Number, "", "-0.5").unwrap(),
            CellValue::Number(-0.5)
        );
        for bad in ["inf", "-inf", "NaN", "2,5", "two", "3px"] {
            let err = parse_one(PropertyKind::Number, "", bad).unwrap_err();
            assert!(err.message().contains(bad.trim()), "{err}");
        }
    }

    #[test]
    fn a_date_takes_the_stored_shape_and_nothing_else() {
        assert_eq!(
            parse_one(PropertyKind::Date, "", "2026-09-22").unwrap(),
            CellValue::Text("2026-09-22".into())
        );
        assert_eq!(
            parse_one(PropertyKind::Date, "", " 2026-09-22T14:03 ").unwrap(),
            CellValue::Text("2026-09-22T14:03".into()),
            "trimmed to the fixed width the sort depends on"
        );
        // The shape is the gate; the calendar is not (ADR-0069).
        assert_eq!(iso_date("2026-02-30"), Some("2026-02-30".into()));
        for bad in [
            "2026-9-2",
            "22/09/2026",
            "2026-09-22T14",
            "2026-09-22 14:03",
            "2026-13-01",
            "2026-09-32",
            "2026-09-22T24:00",
            "2026-09-22T14:60",
            "today",
        ] {
            assert!(
                parse_one(PropertyKind::Date, "", bad).is_err(),
                "{bad:?} was stored"
            );
            assert_eq!(iso_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_checkbox_takes_words_and_a_false_one_is_a_value() {
        for yes in ["true", "Yes", "on", "x", "CHECKED", " true "] {
            assert_eq!(
                parse_one(PropertyKind::Checkbox, "", yes).unwrap(),
                CellValue::Flag(true),
                "{yes}"
            );
        }
        for no in ["false", "no", "off", "unchecked", "FALSE"] {
            assert_eq!(
                parse_one(PropertyKind::Checkbox, "", no).unwrap(),
                CellValue::Flag(false),
                "{no}"
            );
        }
        // Unchecked is a value, not an absence — and absent is not unchecked.
        assert_ne!(
            parse_one(PropertyKind::Checkbox, "", "no").unwrap(),
            CellValue::Empty
        );
        assert!(parse_one(PropertyKind::Checkbox, "", "maybe").is_err());
    }

    #[test]
    fn the_three_string_kinds_store_what_was_typed_and_only_hint() {
        // Nothing is rewritten: not the case, not the scheme, not the spacing.
        for typed in ["HTTP://Example.COM", "not a url", "www.x", "⎈"] {
            assert_eq!(
                parse_one(PropertyKind::Url, "", typed).unwrap(),
                CellValue::Text(typed.into())
            );
        }
        assert_eq!(
            parse_one(PropertyKind::Email, "", "a@b").unwrap(),
            CellValue::Text("a@b".into())
        );
        assert_eq!(
            parse_one(PropertyKind::Phone, "", "+86 138 0000 0000").unwrap(),
            CellValue::Text("+86 138 0000 0000".into())
        );

        // The hint, which never refuses anything: a URL with no scheme, a
        // mail-ish string with a dotless domain, a four-digit phone.
        assert!(looks_valid(PropertyKind::Url, "https://example.com/a"));
        assert!(looks_valid(PropertyKind::Url, "www.example.com"));
        assert!(!looks_valid(PropertyKind::Url, "example.com"));
        assert!(!looks_valid(PropertyKind::Url, "two words"));
        assert!(looks_valid(PropertyKind::Email, "a@example.com"));
        assert!(!looks_valid(PropertyKind::Email, "a@b"));
        assert!(!looks_valid(PropertyKind::Email, "a@@b.com"));
        assert!(!looks_valid(PropertyKind::Email, "@example.com"));
        assert!(looks_valid(PropertyKind::Phone, "+86 138 0000 0000"));
        assert!(looks_valid(PropertyKind::Phone, "(029) 555-1234"));
        assert!(!looks_valid(PropertyKind::Phone, "1234"));
        assert!(!looks_valid(PropertyKind::Phone, "call me"));
        // An empty string is "nothing to say", not "invalid".
        assert!(looks_valid(PropertyKind::Url, ""));
        // And a kind with no opinion never complains.
        assert!(looks_valid(PropertyKind::Text, "anything at all"));
    }

    #[test]
    fn a_select_cell_stores_the_option_id_and_refuses_a_name_it_does_not_have() {
        assert_eq!(
            parse_one(PropertyKind::Select, STATUS_CONFIG, "Done").unwrap(),
            CellValue::Text("7".into())
        );
        assert_eq!(
            parse_one(PropertyKind::Status, STATUS_CONFIG, "done").unwrap(),
            CellValue::Text("7".into()),
            "the name folds case; the id it stores does not"
        );
        assert_eq!(
            parse_one(PropertyKind::Select, STATUS_CONFIG, "7")
                .unwrap_err()
                .message(),
            "column \"select\" has no option \"7\"",
            "typing an id is not how a name is spelled"
        );
        // A column with no options at all refuses every name: nothing may
        // invent an id (that is `option_named`'s job, in a batch of its own).
        assert!(parse_one(PropertyKind::Select, "", "Done").is_err());
    }

    #[test]
    fn a_list_cell_takes_names_or_attachment_ids() {
        assert_eq!(
            parse_many(
                PropertyKind::MultiSelect,
                STATUS_CONFIG,
                &["Done".to_string(), "Doing".to_string()]
            )
            .unwrap(),
            CellValue::Items(vec!["7".into(), "2".into()]),
            "the order the user picked is the order stored"
        );
        assert_eq!(
            parse_many(PropertyKind::Files, "", &["12".to_string()]).unwrap(),
            CellValue::Items(vec!["12".into()])
        );
        for bad in ["0", "", "007", "+3", "file.pdf"] {
            assert!(
                parse_many(PropertyKind::Files, "", &[bad.to_string()]).is_err(),
                "{bad:?} was stored as an attachment id"
            );
        }
        assert!(parse_many(
            PropertyKind::MultiSelect,
            STATUS_CONFIG,
            &["Waiting".to_string()]
        )
        .is_err());
        // The wrong door, each way round.
        assert!(parse_one(PropertyKind::Files, "", "12").is_err());
        assert!(parse_many(PropertyKind::Text, "", &["a".to_string()]).is_err());
    }

    #[test]
    fn a_column_that_stores_nothing_refuses_every_write() {
        for kind in [
            PropertyKind::CreatedTime,
            PropertyKind::LastEditedTime,
            PropertyKind::Formula,
            PropertyKind::Rollup,
        ] {
            let err = parse_one(kind, "", "anything").unwrap_err();
            assert!(err.message().contains(kind.as_str()), "{kind:?} said {err}");
            // "Clear it" is not a write either: the derivation would put it
            // straight back, and a row nothing reads is a row that lies.
            assert!(parse_one(kind, "", "").is_err(), "{kind:?}");
        }
    }

    /// A relation is the one kind that moved out of the group above (ADR-0088):
    /// it stores a *list* of target ids, so it refuses one value the way the
    /// other two list kinds do — and an empty list is `Empty`, which is a
    /// legitimate "nothing is related", not a refused write.
    #[test]
    fn a_relation_takes_a_list_of_record_ids_and_an_empty_list_is_no_value() {
        assert_eq!(
            parse_many(PropertyKind::Relation, "", &[]).unwrap(),
            CellValue::Empty
        );
        assert_eq!(
            parse_many(PropertyKind::Relation, "", &["12".into(), "13".into()]).unwrap(),
            CellValue::Items(vec!["12".into(), "13".into()])
        );
        // The one door, each way round.
        assert!(parse_one(PropertyKind::Relation, "", "12").is_err());
        assert!(parse_many(PropertyKind::Relation, "", &["12".into()]).is_ok());
        // Canonical digits only, exactly as a files cell demands of an
        // attachment id: the stored string is the id's one spelling.
        for bad in ["", " ", "007", "+3", "-1", "0", "twelve", "1.0"] {
            assert!(
                parse_many(PropertyKind::Relation, "", &[bad.to_string()]).is_err(),
                "{bad:?} was accepted as a record id"
            );
        }
    }

    // ─── the paint path ────────────────────────────────────────────────────

    #[test]
    fn a_cell_is_painted_through_its_columns_settings() {
        assert_eq!(
            paint(
                PropertyKind::Select,
                STATUS_CONFIG,
                &CellValue::Text("7".into()),
                &()
            ),
            "Done"
        );
        // An id the column does not list paints itself: the value is still
        // there, and a blank cell would say it was not.
        assert_eq!(
            paint(
                PropertyKind::Select,
                STATUS_CONFIG,
                &CellValue::Text("9".into()),
                &()
            ),
            "9"
        );
        assert_eq!(
            paint(PropertyKind::Select, "", &CellValue::Text("9".into()), &()),
            "9"
        );
        assert_eq!(
            paint(
                PropertyKind::MultiSelect,
                STATUS_CONFIG,
                &CellValue::Items(vec!["2".into(), "7".into()]),
                &()
            ),
            "Doing, Done"
        );
        assert_eq!(
            paint(
                PropertyKind::Number,
                r#"{"format":"percent"}"#,
                &CellValue::Number(0.25),
                &()
            ),
            "25%"
        );
        // A kind with no settings paints the value's own form, so the cell and
        // `display` cannot disagree about a number.
        assert_eq!(
            paint(PropertyKind::Number, "", &CellValue::Number(3.0), &()),
            "3"
        );
        assert_eq!(
            paint(PropertyKind::Text, "", &CellValue::Text("hi".into()), &()),
            "hi"
        );
        assert_eq!(paint(PropertyKind::Text, "", &CellValue::Empty, &()), "");
        assert_eq!(
            paint(PropertyKind::Checkbox, "", &CellValue::Flag(false), &()),
            "No"
        );
        assert_eq!(
            paint(
                PropertyKind::Date,
                "",
                &CellValue::Text("2026-09-22T14:03".into()),
                &()
            ),
            "2026-09-22"
        );
        assert_eq!(
            paint(
                PropertyKind::CreatedTime,
                "",
                &CellValue::Text("2026-09-22T14:03".into()),
                &()
            ),
            "2026-09-22T14:03"
        );
    }

    #[test]
    fn a_files_cell_paints_the_names_it_is_given_and_its_ids_when_it_is_not() {
        let mut names = HashMap::new();
        names.insert("12".to_string(), "report.pdf".to_string());
        let value = CellValue::Items(vec!["12".into(), "13".into()]);
        assert_eq!(
            paint(PropertyKind::Files, "", &value, &names),
            "report.pdf, 13",
            "a name when we have one, the id when we do not"
        );
        assert_eq!(
            paint(PropertyKind::Files, "", &value, &()),
            "12, 13",
            "`()` is the honest nobody-looked: the ids"
        );
        assert_eq!(paint(PropertyKind::Files, "", &CellValue::Empty, &names), "");
    }

    #[test]
    fn a_column_answers_for_its_own_cells() {
        let status = column(PropertyKind::Status, STATUS_CONFIG);
        assert_eq!(status.options().len(), 2);
        assert_eq!(status.parse("Done").unwrap(), CellValue::Text("7".into()));
        assert!(status.parse("Nope").is_err());
        assert_eq!(status.paint(&CellValue::Text("2".into()), &()), "Doing");
        let files = column(PropertyKind::Files, "");
        assert_eq!(
            files.parse_many(&["3".to_string()]).unwrap(),
            CellValue::Items(vec!["3".into()])
        );
    }
}
