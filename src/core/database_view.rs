// Database views (SPEC §三十九 「视图」/「操作」, ADR-0060/0064) — the pure half of
// D3: which columns a view shows, how wide each one is, and what one window of
// rows looks like to the delegate that draws it.
//
// Nothing here reads SQL, talks to Slint or decides an order. The split is
// D0's, restated for the drawing phase:
//
// * the *window* is `core::database::window` (how many rows exist as objects),
//   and the rows themselves come from `storage::database_store`, which already
//   paints every cell through its own column (`database_property::paint`);
// * *what this module owns* is the view's definition as a document (ADR-0064:
//   visible columns and widths are keys in one JSON blob), the shape one
//   painted row takes on its way to a Slint model, and the arithmetic that
//   turns a drag on a column edge into a stored width.
//
// The one rule that shapes everything below: **the delegate must stay cheap**.
// A database is the only thing in this app that can put ten thousand rows
// beside twenty columns, so a row arrives here already painted, already ordered
// and already windowed, and the Slint side only lays out `Text` and shapes
// (ARCHITECTURE rule 6). Anything that would make a delegate do work — parsing
// a config, naming an option, choosing a date format — is done here, once per
// projection, and never per frame; the one config parse there is happens once
// per *column*, so a hundred rows of a select column share one answer.
//
// D3 draws `table` and nothing else (SPEC's order is the implementation order),
// so a view of another layout is *recognised* and refused by name rather than
// drawn wrongly: `LayoutSupport::Missing` carries the label the block says out
// loud, and D5 turns one arm of it into a real view per phase.

use super::database::{
    DatabaseCatalog, DatabaseId, Property, PropertyId, PropertyKind, RowWindow, RowView, View,
    ViewId, ViewLayout,
};
use super::database_property::{json::Json, PropertyOptions};
use super::types::PageId;

/// The denominator of a stored column width: a width is **permille of the
/// grid's own width**, so a view keeps its proportions when the window is
/// resized and a stored definition means the same thing on another machine with
/// another font. ADR-0064 stores widths as a map keyed by property id for
/// exactly the reason this is a permille and not a pixel: a pixel is a fact
/// about one window, and a view's document has to outlive the window it was
/// written in.
pub const WIDTH_UNIT: u16 = 1000;

/// The narrowest a column may be dragged. Sixty permille is about one twentieth
/// of the grid: below that a column cannot show even an elided word, and a
/// column a user cannot read is a column they cannot get back (the resize
/// handle lives inside the header it belongs to). It is also the floor that
/// keeps "the sum of the widths" bounded — sixteen columns at the floor still
/// leave the grid's width to the last column — so the table can always divide
/// what is left without any column going negative.
pub const WIDTH_MIN: u16 = 60;

/// A stored width that means "no opinion": the column takes an equal share of
/// the grid. Zero rather than a sentinel like `-1` because a width is stored in
/// JSON as a number and 0 is already not a legal drag (the floor is
/// [`WIDTH_MIN`]), so "auto" and "dragged as narrow as possible" can never be
/// confused.
pub const WIDTH_AUTO: u16 = 0;

/// Whether this build draws a layout, or only knows its name. SPEC §三十九's
/// 「顺序即实现顺序」 means the other seven become drawable one phase at a time,
/// and a view whose layout this build cannot draw still **opens**: it says
/// which view it is instead of showing a table that is not what the user asked
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutSupport {
    /// D3 delivers this one: the table.
    Drawn,
    /// Recognised, named, not drawn yet (D5).
    Missing,
}

impl LayoutSupport {
    pub fn of(layout: ViewLayout) -> LayoutSupport {
        match layout {
            ViewLayout::Table => LayoutSupport::Drawn,
            ViewLayout::Board
            | ViewLayout::List
            | ViewLayout::Calendar
            | ViewLayout::Gallery
            | ViewLayout::Timeline
            | ViewLayout::Form
            | ViewLayout::Chart => LayoutSupport::Missing,
        }
    }

    pub fn is_drawn(self) -> bool {
        self == LayoutSupport::Drawn
    }
}

/// ADR-0064's `definition` document, read and written.
///
/// **It is kept as the parsed document, not as a struct of its fields**, and
/// that is the whole point of this type: this build owns two keys (`columns`
/// and `widths`) and *passes every other key through untouched*. A definition
/// written by a later build — one that knows ADR-0064's `filter` and `sorts` —
/// keeps its rules when a D3 user hides a column, instead of having them eaten
/// by a writer that did not understand them. The alternative shapes are both
/// worse: a struct would silently drop the keys it has no field for, and
/// re-serialising only the known keys would make "hide a column" quietly clear
/// a filter.
///
/// A document that does not parse, or that parses to something other than an
/// object, degrades to **no rules** rather than to an error (ADR-0064): a
/// database that cannot be opened is worse than one that is not filtered, and
/// the same fold covers a truncated file and a hand-edited one.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewDefinition {
    document: Json,
}

impl Default for ViewDefinition {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewDefinition {
    /// The document a view with no rules has, which is also what ADR-0064
    /// stores for one (`definition = ''` on a fresh view): an empty object.
    pub fn new() -> Self {
        ViewDefinition {
            document: Json::Object(Vec::new()),
        }
    }

    /// Read a stored document. Never fails: everything unreadable is the empty
    /// document, so "no rules" and "rules this build cannot read" behave the
    /// same way (the view opens showing everything).
    pub fn parse(text: &str) -> Self {
        if text.trim().is_empty() {
            return Self::new();
        }
        match Json::parse(text) {
            // An object is a document — whatever keys it has, including keys
            // this build has never heard of (`put` keeps them).
            Ok(document @ Json::Object(_)) => ViewDefinition { document },
            // A bare `[]`, a number, a truncated blob: not a document. `"v":1`
            // is the version key a later build reads; this one neither reads nor
            // rewrites it, which is what keeps it meaningful.
            _ => Self::new(),
        }
    }

    /// The document as it is stored — the keys this build rewrote, then every
    /// other key it found, in the order it found them.
    pub fn to_text(&self) -> String {
        self.document.to_text()
    }

    /// The stored visible-column list, or `None` when the view has no opinion.
    ///
    /// `None` and an explicit list of everything are deliberately different:
    /// `None` means "show me every column, including ones added later", which is
    /// what a view nobody has configured should mean, while a list is a snapshot
    /// the user made and must not grow when D5 adds a column.
    pub fn column_list(&self) -> Option<Vec<PropertyId>> {
        let ids = self.document.get("columns")?.as_array()?;
        Some(ids.iter().filter_map(Json::as_u64).map(PropertyId).collect())
    }

    fn widths(&self) -> Vec<(PropertyId, u16)> {
        let Some(Json::Object(fields)) = self.document.get("widths") else {
            return Vec::new();
        };
        fields
            .iter()
            .filter_map(|(key, value)| {
                let property = key.parse::<u64>().ok()?;
                let value = value.as_u64()?;
                Some((PropertyId(property), value.min(u16::MAX as u64) as u16))
            })
            .collect()
    }

    /// The width a column was dragged to, or [`WIDTH_AUTO`] when nobody has
    /// dragged it. A width below the floor reads as the floor: a hand-edited
    /// document cannot make a column unreadable by asking for a width of one.
    pub fn width(&self, property: PropertyId) -> u16 {
        self.widths()
            .into_iter()
            .find(|(id, _)| *id == property)
            .map(|(_, width)| {
                if width == WIDTH_AUTO {
                    WIDTH_AUTO
                } else {
                    width.max(WIDTH_MIN)
                }
            })
            .unwrap_or(WIDTH_AUTO)
    }

    /// Remember a column's new width. A width at or below the floor stores the
    /// floor, so a stored value is always drawable; [`WIDTH_AUTO`] is not
    /// reachable from a drag and is how a column goes back to its share.
    pub fn set_width(&mut self, property: PropertyId, width: u16) {
        let width = if width == WIDTH_AUTO {
            WIDTH_AUTO
        } else {
            width.clamp(WIDTH_MIN, WIDTH_UNIT + WIDTH_MIN)
        };
        let mut widths: Vec<(String, Json)> = self
            .widths()
            .into_iter()
            .filter(|(id, _)| *id != property)
            .map(|(id, value)| (id.as_u64().to_string(), Json::Number(value as f64)))
            .collect();
        if width != WIDTH_AUTO {
            widths.push((property.as_u64().to_string(), Json::Number(width as f64)));
        }
        self.put("widths", Json::Object(widths));
    }

    /// Hide one column: remove it from the visible list, materialising the list
    /// from `all` when the view had none (so the first hide does not also
    /// silently become "show exactly these").
    pub fn hide(&mut self, property: PropertyId, all: &[PropertyId]) {
        let mut list = self.column_list().unwrap_or_else(|| all.to_vec());
        list.retain(|id| *id != property);
        self.put_columns(list);
    }

    /// Show one column again. It goes back where `all` says it belongs rather
    /// than at the end: the column order is the schema's (`db_properties.ord`,
    /// ADR-0061), and a re-shown column that jumped to the end would be a second
    /// order for the same fact.
    pub fn show(&mut self, property: PropertyId, all: &[PropertyId]) {
        let mut list = self.column_list().unwrap_or_else(|| all.to_vec());
        if list.contains(&property) {
            return;
        }
        list.retain(|id| all.contains(id));
        let at = all.iter().position(|id| *id == property).unwrap_or(all.len());
        let before = all.iter().take(at).filter(|id| list.contains(id)).count();
        list.insert(before.min(list.len()), property);
        self.put_columns(list);
    }

    /// Whether this view shows a column, given every column the database has.
    pub fn shows(&self, property: PropertyId, all: &[PropertyId]) -> bool {
        match self.column_list() {
            Some(list) => list.contains(&property),
            None => all.contains(&property),
        }
    }

    fn put_columns(&mut self, list: Vec<PropertyId>) {
        self.put(
            "columns",
            Json::Array(
                list.into_iter()
                    .map(|id| Json::Number(id.as_u64() as f64))
                    .collect(),
            ),
        );
    }

    /// Set one key, keeping every other key exactly where it was. An empty
    /// `widths` map is stored as an empty object rather than removed: the key is
    /// this build's, and "I own this key and it is empty" is a different
    /// statement from "I have never heard of it".
    fn put(&mut self, key: &str, value: Json) {
        let Json::Object(fields) = &mut self.document else {
            return;
        };
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => fields.push((key.to_string(), value)),
        }
    }
}

/// The columns one view shows, in the order it shows them.
///
/// Three rules, in this order and no other:
///
/// 1. the **title column is always first** and is never hidden — it is the
///    column a row is *named* by (ADR-0063: its value is `pages.title` for a
///    page-backed record), so a table without it is a list of anonymous rows.
///    D3 does not offer the toggle for it at all, and this filter is the second
///    half of that promise: a hand-edited document cannot hide it either.
/// 2. the rest follow `columns` when the view has a list, and the schema's own
///    `ord` when it does not;
/// 3. **an id this database does not have is dropped** (ADR-0064): no foreign
///    key reaches inside a JSON document, so a view whose list names a deleted
///    property loses that column and shows the rest instead of failing to open.
///
/// The properties come back cloned and in view order because that *is* the
/// table's shape: the projection paints cells against them, the delegate lays
/// out one column each, and the Markdown export writes them as the header row
/// (ADR-0065). A database has a handful of columns and never a row per column.
pub fn view_columns(catalog: &DatabaseCatalog, db: DatabaseId, view: &View) -> Vec<Property> {
    let schema: Vec<Property> = catalog.properties_of(db).cloned().collect();
    let definition = ViewDefinition::parse(&view.definition);
    let title = schema.iter().find(|p| p.kind.is_title()).cloned();

    let visible: Vec<Property> = match definition.column_list() {
        Some(list) => list
            .iter()
            .filter(|id| title.as_ref().map(|t| t.id) != Some(**id))
            .filter_map(|id| schema.iter().find(|p| p.id == *id).cloned())
            .collect(),
        None => schema.iter().filter(|p| !p.kind.is_title()).cloned().collect(),
    };

    let mut out = Vec::with_capacity(visible.len() + 1);
    if let Some(title) = title {
        out.push(title);
    }
    out.extend(visible);
    out
}

/// Every property of a database id, in schema order — what the columns popup
/// lists, and what a hide/show edit is relative to.
pub fn all_columns(catalog: &DatabaseCatalog, db: DatabaseId) -> Vec<PropertyId> {
    catalog.properties_of(db).map(|p| p.id).collect()
}

/// One option of a select / status column, as the cell's own dropdown draws it.
/// D3 shows the list inside the cell rather than in a window-level popup,
/// because a popup per cell would be one popup per realized row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionChoice {
    /// The **stored** value (an option id, ADR-0061): what a pick writes.
    pub id: String,
    /// What the row draws (`Done`), which is what the pick shows.
    pub name: String,
    /// The option's palette slot, `""` for none. Paint only — this file does
    /// not know what a colour means (hard rule 3), it only carries the word.
    pub color: String,
}

/// One column as the table draws it: what the header says, what a cell of it is
/// edited as, and how wide it is.
#[derive(Debug, Clone, PartialEq)]
pub struct TableColumn {
    pub property: PropertyId,
    /// The header's text (`db_properties.name`).
    pub name: String,
    /// The type, which is the *delegate's* only decision: a text cell is a
    /// `Text` that becomes the one live input, a checkbox is a box, a select is
    /// a pill that opens its own options. D3 draws title / text / number /
    /// checkbox inline and every other kind read-only, and this is the word that
    /// says which of those a cell is.
    pub kind: PropertyKind,
    /// Permille of the grid width, or [`WIDTH_AUTO`] for an equal share.
    pub width: u16,
    /// The title column: the one a row is named by, and the one whose editor is
    /// the row's name rather than a grid cell.
    pub title: bool,
    /// A select / status column's options, read once per column per projection
    /// from the property's `config` (ADR-0061's one bit of JSON in the schema).
    /// Empty for every other kind, and for a column with no options configured —
    /// which is a select with nothing to pick, a state D5's option editor has to
    /// be able to leave. The cell carries no copy: a pick cell looks its own
    /// column up by index.
    pub options: Vec<OptionChoice>,
}

/// One cell as the delegate draws it. Everything here is already painted, so a
/// cell costs a `Text` and a comparison — never a config parse.
#[derive(Debug, Clone, PartialEq)]
pub struct TableCellView {
    pub property: PropertyId,
    pub kind: PropertyKind,
    /// The cell's text, as `database_property::paint` wrote it (the store paints
    /// on the way out of SQL, so this is not painted twice).
    pub painted: String,
    /// A checkbox's state. Derived from [`FLAG_TRUE`] and nowhere else: the
    /// painted word is ADR-0065's (`Yes`/`No`, so a cell and the exported table
    /// agree), and `CellValue::Flag(true).display()` is the one place that word
    /// is spelled — which is what lets the delegate stop comparing strings.
    pub checked: bool,
    /// Whether this cell can be edited in place. False for the three computed
    /// kinds (`formula` / `rollup` / `relation`) and the two derived ones
    /// (`created time` / `last edited time`), which nothing writes: clicking one
    /// must not open an editor whose every keystroke would be refused.
    pub editable: bool,
}

/// The word a checkbox's painted form has, in one place: `CellValue::Flag(true)`
/// renders as this (ADR-0065), and [`TableCellView::checked`] is derived from it.
pub const FLAG_TRUE: &str = "Yes";

impl TableCellView {
    /// Whether this is a checkbox cell — the question the delegate's arm asks,
    /// so "a checkbox" has one answer instead of a kind comparison repeated in
    /// three places.
    pub fn is_flag(&self) -> bool {
        self.kind == PropertyKind::Checkbox
    }

    /// Whether this cell picks from a list of options (select / status): the
    /// other kind D3 draws as something other than text.
    pub fn is_pick(&self) -> bool {
        matches!(self.kind, PropertyKind::Select | PropertyKind::Status)
    }
}

/// One row of the table: the identity its cells are edited against, the page the
/// row may be (ADR-0063), and one cell per visible column in column order.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRowView {
    pub record: u64,
    /// `Some` when the record owns a page: the row's title is `pages.title`, and
    /// "Open" navigates there instead of making one. `None` is a bare record —
    /// most of them, because a page is created lazily (ADR-0063).
    pub page: Option<PageId>,
    /// The first column's text (ADR-0063's `COALESCE`), on the row rather than
    /// in the cells because a row's *name* is a different thing from a cell: it
    /// is what the row menu, the export's first column and the Open action say.
    pub title: String,
    pub cells: Vec<TableCellView>,
}

/// One tab of the view switcher. D3 has exactly one view per database, and the
/// switcher's *shape* is the point: it lists `db_views` rows by name and marks
/// the active one, so D5 adds a second view without changing anything the
/// delegate reads.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewTab {
    pub view: ViewId,
    pub name: String,
    pub layout: ViewLayout,
    pub active: bool,
}

/// One database block's whole projection: the tabs, the columns, and the rows
/// of the window that is currently realized. It is built in Rust and pushed
/// into the Slint model as it is; the delegate reads it and does no work.
#[derive(Debug, Clone, PartialEq)]
pub struct TableView {
    pub tabs: Vec<ViewTab>,
    pub columns: Vec<TableColumn>,
    pub rows: Vec<TableRowView>,
    /// The database's own row count (`COUNT(*)`), which is what the view's
    /// scroll surface is made of. The rows above are only the window.
    pub total: usize,
    /// Which slice of `total` the rows are, so the delegate places row `i` at its
    /// true offset in the scroll surface instead of at the top.
    pub window: RowWindow,
    /// The active view's layout, and whether this build draws it. A view whose
    /// layout is not drawn still lists its tabs, still counts its rows, and says
    /// which view it is instead of pretending to be a table.
    pub layout: ViewLayout,
    pub support: LayoutSupport,
}

impl TableView {
    /// The row height the whole view is laid out at, in px. Fixed, and stated
    /// once: the window arithmetic (`core::database::window`) divides the scroll
    /// offset by it, so a view that laid its rows out at another height would
    /// fetch a window that does not cover the viewport. It matches the table
    /// grid's own body line for the same reason — the two kinds sit in one page,
    /// and a row of each should read as a row.
    pub const ROW_HEIGHT: f32 = 32.0;

    /// The header's height: the switcher line plus the column-header line. Part
    /// of the block's own height and *not* part of the scroll surface, which is
    /// why it is a constant here rather than a measurement Slint hands back.
    pub const HEADER_HEIGHT: f32 = 64.0;

    /// How tall the block is: the header plus every row of the database. A block
    /// whose height is the *table's* height is what makes the page's own scroll
    /// the view's scroll — there is no second scroll region, so the realized rows
    /// are the ones the page viewport can see (SPEC §三十九 红线第一条), and a
    /// 10 000-row database is a tall block rather than a nested list.
    ///
    /// Known boundary, and the price of that shape: an inline database of ten
    /// thousand rows makes its page 320 000 px tall. Capping it needs a scroll
    /// region of its own inside the page, which is a second wheel target inside
    /// the first — D5's call to make, with a number, if a user ever complains.
    pub fn height(&self) -> f32 {
        Self::HEADER_HEIGHT + self.total as f32 * Self::ROW_HEIGHT
    }

    /// Whether the database has no rows at all: the table then draws its header
    /// and its one "new row" line, which is what an empty database has to look
    /// like if anyone is going to fill it.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The `y` of row `index` inside the block, in px — the absolute position of
    /// one realized row in the scroll surface, which is `(window.start + i)`
    /// rows down. This is the row→model arithmetic §三十七 requires of every
    /// "rows are dynamic" block, done once, here, instead of in a delegate: the
    /// model index and the row index differ by `window.start`, and nothing in
    /// the Slint side is allowed to guess which one it has.
    pub fn row_y(&self, index: usize) -> f32 {
        Self::HEADER_HEIGHT + (self.window.start + index) as f32 * Self::ROW_HEIGHT
    }
}

/// One painted row from the store plus the one thing a window read does not
/// carry: the page a record owns (ADR-0063). The Markdown export needs it to
/// write `[title](quire://page/<id>)` for a page-backed row (ADR-0065), and it is
/// a map rather than a field of the row type so the read path keeps its one
/// shape — `window_rows` returns `RowView`s, and only the export looks here.
pub type RecordPages = std::collections::HashMap<u64, PageId>;

/// Assemble one window of rows into the table's shape.
///
/// `rows` are the window's rows as the store painted them, in SQL's order (the
/// only order there is); `columns` is [`view_columns`]' answer. The zip is by
/// position and cannot be short: the store's read builds one cell per requested
/// column, in the same order this walks, which is why the two take the same list.
pub fn table_rows(rows: &[RowView], columns: &[TableColumn], pages: &RecordPages) -> Vec<TableRowView> {
    rows.iter()
        .map(|row| TableRowView {
            record: row.record,
            page: pages.get(&row.record).copied(),
            // The store paints the title column into `RowView::title`, and also
            // into the first cell when the title column is visible; the row's own
            // field is the one both cases agree on (ADR-0063).
            title: row.title.clone(),
            cells: columns
                .iter()
                .enumerate()
                .map(|(at, column)| {
                    let painted = row.cells.get(at).cloned().unwrap_or_default();
                    TableCellView {
                        property: column.property,
                        kind: column.kind,
                        checked: painted == FLAG_TRUE,
                        editable: !column.kind.is_computed() && !column.kind.is_derived(),
                        painted,
                    }
                })
                .collect(),
        })
        .collect()
}

/// The columns a table draws, from the properties a view shows: the header's
/// words, the kind each cell is edited as, the stored width, and the options a
/// pick cell needs. This is where a `Property` becomes a `TableColumn`, and it is
/// the *only* place that reads a column's `config` — once per column.
pub fn table_columns(properties: &[Property], definition: &ViewDefinition) -> Vec<TableColumn> {
    properties
        .iter()
        .map(|property| TableColumn {
            property: property.id,
            name: property.name.clone(),
            kind: property.kind,
            width: definition.width(property.id),
            title: property.kind.is_title(),
            options: options_of(property),
        })
        .collect()
}

/// A select / status column's options, read out of its `config` and turned into
/// what the cell's dropdown draws. Empty for every other kind (a column with an
/// option list that is not a pick is a config this build ignores, which is
/// ADR-0069's fold: an unknown setting is not an error).
pub fn options_of(property: &Property) -> Vec<OptionChoice> {
    if !matches!(property.kind, PropertyKind::Select | PropertyKind::Status) {
        return Vec::new();
    }
    let options: PropertyOptions = property.options();
    options
        .iter()
        .map(|option| OptionChoice {
            id: option.id.as_u64().to_string(),
            name: option.name.clone(),
            color: option.color.clone(),
        })
        .collect()
}
