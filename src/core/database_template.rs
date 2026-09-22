// The database record template (SPEC §三十九 「操作」's 数据库模板, ADR-0086).
//
// A database's template is the prefill every new record starts from, stored as
// one JSON document in the `databases` row itself (`databases.template`, v20):
//
//     {"cells":{"7":"Design review","9":true,"3":"opt-2"}}
//
// Every value is in the **exact shape [`CellValue`] stores** — a string for the
// text-bearing kinds (title, text, url, email, phone, date, a select's option
// id), a number, a boolean for the checkbox, an array of strings for the two
// list kinds. That is the whole point of this module's smallness: the brief's
// rule for templates on both tracks (T1's page templates and this one) is
// 「模板是内容的副本、不引入第二套内容格式」, and for a record the content
// format the store already uses *is* [`CellValue`]'s three columns plus the
// items table — so a template cell is a stored cell written down, and
// applying a template is the same `SetDatabaseCell` write a keystroke makes,
// parsed by nothing and converted by nothing.
//
// What is deliberately *not* here: any notion of a template *record* (Notion's
// template gallery is a set of rows marked as templates — a stored flag and a
// projection rule this build does not need), any per-view template (a template
// is about the records a database *creates*, and every layout creates them the
// same way), and any validation beyond the shape check — the values were
// already valid when the row they were copied from was written, and a
// hand-edited document's bad value fails the same way a bad keystroke does:
// that one cell is not written (`cells_of` skips it), so the prefill never
// writes a value the column's kind cannot hold.

use super::database::{CellValue, PropertyId};

/// The cells a template carries: `(property id, stored value)`, in document
/// order — which is the order the row the template was copied from was read
/// in, and the order the prefill writes them in. A parsed-but-unusable entry
/// never appears here; the document is the only copy and nothing needs to
/// count what it dropped.
pub type TemplateCells = Vec<(PropertyId, CellValue)>;

/// The JSON value shapes a template cell takes, matched against what
/// `core::database_property::json` parses. Written here rather than imported
/// because the *mapping* (string → `CellValue::Text`, array → `Items`) is the
/// template's own contract, not the JSON reader's.
fn value_of_json(json: &super::database_property::json::Json) -> CellValue {
    use super::database_property::json::Json;
    match json {
        Json::Null => CellValue::Empty,
        Json::Text(text) => CellValue::Text(text.clone()),
        Json::Number(number) => CellValue::Number(*number),
        Json::Bool(flag) => CellValue::Flag(*flag),
        Json::Array(items) => CellValue::Items(
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect(),
        ),
        // An object is not a value shape any kind stores; it is the fold every
        // unreadable setting takes (ADR-0069): skipped by the caller, never an
        // error.
        Json::Object(_) => CellValue::Empty,
    }
}

/// One template cell's JSON, in the shape [`value_of_json`] reads back. An
/// [`CellValue::Empty`] writes nothing (the key is dropped — "no value" is the
/// absence of the row, ADR-0062's one representation, and a template that
/// carries an explicit empty would make every prefill write a no-op row).
fn json_of_value(value: &CellValue) -> Option<super::database_property::json::Json> {
    use super::database_property::json::Json;
    match value {
        CellValue::Empty => None,
        CellValue::Text(text) => Some(Json::Text(text.clone())),
        CellValue::Number(number) => Some(Json::Number(*number)),
        CellValue::Flag(flag) => Some(Json::Bool(*flag)),
        CellValue::Items(items) => Some(Json::Array(
            items.iter().map(|item| Json::Text(item.clone())).collect(),
        )),
    }
}

/// Read a stored template document into its cells. Never fails: a document
/// that does not parse, is not an object, or whose `cells` is not an object of
/// legal values degrades to **no template** (the same fold ADR-0064's view
/// documents take) — a record nobody templated and a record whose template
/// broke behave identically, and neither fails the row it was meant to prefill.
pub fn cells_of(template: &str) -> TemplateCells {
    use super::database_property::json::Json;
    let document = match Json::parse(template) {
        Ok(document @ Json::Object(_)) => document,
        _ => return Vec::new(),
    };
    let Some(Json::Object(fields)) = document.get("cells") else {
        return Vec::new();
    };
    fields
        .iter()
        .filter_map(|(key, value)| {
            let property = key.parse::<u64>().ok()?;
            // An empty text cell is a template that prefills that column with
            // `""` — a *row* the user can see and clear, and a real prefill;
            // `Empty` is what the key's absence means, so only `Null` and the
            // object fold skip a cell.
            if matches!(value, Json::Null | Json::Object(_)) {
                return None;
            }
            Some((PropertyId(property), value_of_json(value)))
        })
        .collect()
}

/// Build a template document from a row's cells — the write the "save this row
/// as the template" action asks for. Values that carry no cell (`Empty`) are
/// skipped, so a row with nothing filled makes the **empty template**, which
/// is how the affordance clears one: copying an empty row is a copy of
/// nothing, and that is the honest word for "no prefill".
pub fn from_cells(cells: &[(u64, CellValue)]) -> String {
    use super::database_property::json::Json;
    let fields: Vec<(String, Json)> = cells
        .iter()
        .filter_map(|(property, value)| {
            json_of_value(value).map(|json| (property.to_string(), json))
        })
        .collect();
    Json::Object(vec![(
        "cells".to_string(),
        Json::Object(fields),
    )])
    .to_text()
}
