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
// D3 drew `table`, D5 drew the six layouts after it (board, list, calendar,
// gallery, timeline, form) and D7 drew the eighth, `chart` — SPEC's order is
// the implementation order, so the list is complete and the only thing left
// for [`LayoutSupport::Missing`] is a layout a later build invents.

use super::database::{
    DatabaseCatalog, DatabaseId, Property, PropertyId, PropertyKind, RowWindow, RowView, SortSpec,
    View, ViewId, ViewLayout,
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

/// Which layouts this build draws. SPEC §三十九's 「顺序即实现顺序」:
/// D3 delivered `table`, D5 delivered the next six (board / list / calendar
/// / gallery / timeline / form), and D7 delivers `chart` — the last of the
/// eight, which is why [`LayoutSupport::Missing`] has no producer left. The
/// variant stays: a *stored* layout string this build does not know folds to
/// `table` (`ViewLayout::from_stored`), so `Missing` can only mean "a future
/// build added a ninth layout", and the fold that keeps that honest lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutSupport {
    /// D3's table, D5's six, and D7's chart: drawn by the block's delegate.
    Drawn,
    /// Recognised, named, not drawn yet (no layout reaches it in this build).
    Missing,
}

impl LayoutSupport {
    pub fn of(layout: ViewLayout) -> LayoutSupport {
        match layout {
            ViewLayout::Table
            | ViewLayout::Board
            | ViewLayout::List
            | ViewLayout::Calendar
            | ViewLayout::Gallery
            | ViewLayout::Timeline
            | ViewLayout::Form
            | ViewLayout::Chart => LayoutSupport::Drawn,
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

// ─── D4: the view's rules (filter / sort / group) ────────────────────────────
//
// ADR-0064 put a view's rules in one JSON document because SQL never filters on
// *them* — it filters on the values, through the query the rules are compiled
// into. D3 owned two keys of that document (`columns`, `widths`) and passed the
// rest through untouched (ADR-0074). D4 owns the other three: `filter`,
// `sorts`, `groups`.
//
// The split this section is the first half of:
//
//     the document (JSON)                       ← here: parsed, typed, degraded
//          │  ViewRules
//          ▼
//     storage::database_query  (the SQL)         ← the WHERE / ORDER BY / GROUP BY
//          │  one statement
//          ▼
//     the window (`core::database::window`)      ← the same window as D0's
//
// Nothing here writes SQL and nothing here reads a row: this half is pure, so
// "what did the user ask for" can be tested without a file, and the red line
// (filter / sort in SQL, not in the UI) is a boundary between two modules
// rather than a rule someone has to remember.
//
// **The degradation rules are decisions, and they are stated once, here**:
//
// * the document does not parse, or its `filter` is not a shape this build can
//   read (a group whose children are not an array, nesting past
//   [`FILTER_MAX_DEPTH`]) → **the whole filter is dropped** and `note` says so
//   on screen. A view that cannot be opened is worse than one that is not
//   filtered (ADR-0064), and a filter that was *silently* ignored is worse than
//   both — it shows rows the user did not ask for with nothing to explain them.
// * a single clause names a column that is gone, a column whose kind cannot be
//   compared that way (`contains` on a number), or a value that is not the
//   shape its column stores (`"next tuesday"` as a date) → **that clause** is
//   dropped and counted in `note`. This is ADR-0064's rule for a deleted
//   property — the view loses the clause and shows more rows instead of failing
//   to open — applied to the other ways one clause can be unreadable.
// * a sort term or a group that cannot be compiled is dropped **quietly**: an
//   order or a grouping is a way of *looking* at rows, never a way of hiding
//   them, so the honest failure ("this is not sorted the way the document says")
//   is visible in the first frame rather than needing a sentence.
// * an explicitly empty group (`{"and":[]}`, what the panel leaves behind when
//   its last rule is deleted) is **no filter at all** and produces no note: it
//   is the normal state of a view nobody has filtered, not a degradation.

/// The comparisons the filter panel offers, and the only ones the parser accepts
/// for a kind — one list for both, so the menu can never offer something the
/// compiler would refuse.
///
/// Which comparisons a kind has is a decision per kind, and each row of the
/// table is the reason the enum is not just "eq":
///
/// | kind | comparisons | why |
/// |------|-------------|-----|
/// | title / text / url / email / phone | contains, is, is not, is/is-not empty | words are matched by substring; ordering words by bytes is not a question a user asks |
/// | date / created time / last edited time | is, is not, before, on-or-before, after, on-or-after, is/is-not empty | a date is compared as its stored fixed-width text (ADR-0062), so bytes *are* time order |
/// | number | is, is not, `>` `≥` `<` `≤`, is/is-not empty | compared in `db_values.num`, so `2` is less than `10` |
/// | checkbox | is, is/is-not empty | a checkbox has two states and an absence |
/// | select / status | is, is not, is any of, is/is-not empty | values are option **ids** (ADR-0061), so "is any of" is an `IN` over ids and a substring match on a label is not a thing SQL can do over a JSON config |
/// | multi-select / files | has, has any of, is/is-not empty | the value is `db_value_items` rows, so "has" is an `EXISTS` probe (ADR-0062's predicted shape) |
/// | formula / rollup / relation | — | the value is computed at projection time and not stored (ADR-0062), so there is nothing for SQL to compare. D6 may add comparisons that compute the cell first |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterOp {
    /// A substring of the text (or, for a list column, one item it holds).
    Contains,
    /// Is this value — the option **id** for a pick, the number for a number,
    /// the stored shape for a date, the checked state for a checkbox.
    Eq,
    /// Is a value *other* than this one. An empty cell matches neither `Eq` nor
    /// `Ne`: it holds no value to be either.
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    /// One of several option ids (select / status) or items (multi-select /
    /// files).
    AnyOf,
    IsEmpty,
    IsNotEmpty,
}

/// The list's order is the panel's order, and the int is the index — the same
/// rule `PropertyKind`'s ints follow, so adding a comparison appends a number
/// instead of renumbering one.
pub const FILTER_OPS: [FilterOp; 10] = [
    FilterOp::Contains,
    FilterOp::Eq,
    FilterOp::Ne,
    FilterOp::Gt,
    FilterOp::Gte,
    FilterOp::Lt,
    FilterOp::Lte,
    FilterOp::AnyOf,
    FilterOp::IsEmpty,
    FilterOp::IsNotEmpty,
];

impl FilterOp {
    /// The document's word for this comparison. Stable strings, because they
    /// are written into `db_views.definition` and read by later builds.
    pub fn as_str(self) -> &'static str {
        match self {
            FilterOp::Contains => "contains",
            FilterOp::Eq => "eq",
            FilterOp::Ne => "ne",
            FilterOp::Gt => "gt",
            FilterOp::Gte => "gte",
            FilterOp::Lt => "lt",
            FilterOp::Lte => "lte",
            FilterOp::AnyOf => "any-of",
            FilterOp::IsEmpty => "is-empty",
            FilterOp::IsNotEmpty => "is-not-empty",
        }
    }

    pub fn try_from_str(s: &str) -> Option<FilterOp> {
        FILTER_OPS.iter().copied().find(|op| op.as_str() == s)
    }

    /// The index `storage`'s SQL emitter and the panel's int legend agree on.
    pub fn index(self) -> usize {
        FILTER_OPS.iter().position(|op| *op == self).unwrap_or(0)
    }

    pub fn from_index(at: usize) -> Option<FilterOp> {
        FILTER_OPS.get(at).copied()
    }

    /// Whether this comparison asks for a value at all. `is (not) empty` does
    /// not, which is why a clause of one of those with no value is complete.
    pub fn needs_value(self) -> bool {
        !matches!(self, FilterOp::IsEmpty | FilterOp::IsNotEmpty)
    }

    /// The comparisons one kind's panel offers — and, in the parser, the set a
    /// clause of that kind is admissible against.
    pub fn ops_for(kind: PropertyKind) -> &'static [FilterOp] {
        match kind {
            PropertyKind::Title
            | PropertyKind::Text
            | PropertyKind::Url
            | PropertyKind::Email
            | PropertyKind::Phone => &[
                FilterOp::Contains,
                FilterOp::Eq,
                FilterOp::Ne,
                FilterOp::IsEmpty,
                FilterOp::IsNotEmpty,
            ],
            PropertyKind::Date | PropertyKind::CreatedTime | PropertyKind::LastEditedTime => &[
                FilterOp::Eq,
                FilterOp::Ne,
                FilterOp::Lt,
                FilterOp::Lte,
                FilterOp::Gt,
                FilterOp::Gte,
                FilterOp::IsEmpty,
                FilterOp::IsNotEmpty,
            ],
            PropertyKind::Number => &[
                FilterOp::Eq,
                FilterOp::Ne,
                FilterOp::Gt,
                FilterOp::Gte,
                FilterOp::Lt,
                FilterOp::Lte,
                FilterOp::IsEmpty,
                FilterOp::IsNotEmpty,
            ],
            PropertyKind::Checkbox => {
                &[FilterOp::Eq, FilterOp::IsEmpty, FilterOp::IsNotEmpty]
            }
            PropertyKind::Select | PropertyKind::Status => &[
                FilterOp::Eq,
                FilterOp::Ne,
                FilterOp::AnyOf,
                FilterOp::IsEmpty,
                FilterOp::IsNotEmpty,
            ],
            PropertyKind::MultiSelect | PropertyKind::Files => &[
                FilterOp::Contains,
                FilterOp::AnyOf,
                FilterOp::IsEmpty,
                FilterOp::IsNotEmpty,
            ],
            // Computed kinds store nothing to compare (ADR-0062), so they have
            // no comparisons at all — and a clause naming one is a clause the
            // parser drops, with the note saying why.
            PropertyKind::Formula | PropertyKind::Rollup | PropertyKind::Relation => &[],
        }
    }

    /// What the panel's comparison button (and its menu) says. The wording is
    /// per kind where a kind means something different by the same comparison:
    /// "after" is what a date comparison is called, and `>` is what a number's
    /// is.
    pub fn label(self, kind: PropertyKind) -> &'static str {
        let temporal = matches!(
            kind,
            PropertyKind::Date | PropertyKind::CreatedTime | PropertyKind::LastEditedTime
        );
        let list = matches!(kind, PropertyKind::MultiSelect | PropertyKind::Files);
        match self {
            FilterOp::Contains => {
                if list {
                    "has"
                } else {
                    "contains"
                }
            }
            FilterOp::Eq => "is",
            FilterOp::Ne => "is not",
            FilterOp::Gt => {
                if temporal {
                    "after"
                } else {
                    ">"
                }
            }
            FilterOp::Gte => {
                if temporal {
                    "on or after"
                } else {
                    "\u{2265}"
                }
            }
            FilterOp::Lt => {
                if temporal {
                    "before"
                } else {
                    "<"
                }
            }
            FilterOp::Lte => {
                if temporal {
                    "on or before"
                } else {
                    "\u{2264}"
                }
            }
            FilterOp::AnyOf => {
                if list {
                    "has any of"
                } else {
                    "is any of"
                }
            }
            FilterOp::IsEmpty => "is empty",
            FilterOp::IsNotEmpty => "is not empty",
        }
    }
}

/// The value side of one comparison, in the shape the column stores.
///
/// [`FilterValue::Missing`] is "the rule exists but its value is not filled in
/// yet" — what the panel has between "add a rule" and the user typing. It
/// compiles to **no constraint**, deliberately: a half-written rule that hid
/// rows would be a filter the user never asked for, and a panel that could not
/// be left half-written would make "add a rule" a minefield. The document
/// stores it as `null`, so a half-written rule survives a restart as a
/// half-written rule.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    Missing,
    Text(String),
    Number(f64),
    Flag(bool),
    Any(Vec<String>),
}

impl FilterValue {
    /// Whether the panel draws this as a filled-in value. `Missing` (and an
    /// empty `Any`) are what the row paints as "not set yet".
    pub fn is_set(&self) -> bool {
        match self {
            FilterValue::Missing => false,
            FilterValue::Any(items) => !items.is_empty(),
            FilterValue::Text(text) => !text.is_empty(),
            FilterValue::Number(_) | FilterValue::Flag(_) => true,
        }
    }

    /// What the row's value button shows: the value as it is stored. A pick's
    /// ids are turned into option names by the caller (which is the layer that
    /// can read the column's config); this is the honest fallback.
    pub fn display(&self) -> String {
        match self {
            FilterValue::Missing => String::new(),
            FilterValue::Text(text) => text.clone(),
            FilterValue::Number(num) => format!("{num}"),
            FilterValue::Flag(true) => "checked".into(),
            FilterValue::Flag(false) => "unchecked".into(),
            FilterValue::Any(items) => items.join(", "),
        }
    }
}

/// One parsed comparison: which column, how it is compared, and against what.
/// The **kind travels with the clause** because the parser already had to read
/// it to know which comparisons were admissible — so the SQL emitter is a total
/// function over [`FilterClause`] and never has to consult a schema again.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterClause {
    pub property: PropertyId,
    pub kind: PropertyKind,
    pub op: FilterOp,
    pub value: FilterValue,
}

/// ADR-0064's filter tree, parsed. The document's three shapes plus the one
/// this slice added — `{"not":{…}}` — because 与/或/非 is what a filter panel
/// has to be able to say and "neither a nor b" is not expressible with the
/// other two.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterNode {
    /// `{"and":[…]}`
    All(Vec<FilterNode>),
    /// `{"or":[…]}`
    Any(Vec<FilterNode>),
    /// `{"not":{…}}` — one child, since `not` of a set is what `Ne` and
    /// De Morgan already say.
    Not(Box<FilterNode>),
    /// `{"property":7,"op":"eq","value":…}`
    Clause(FilterClause),
}

impl FilterNode {
    /// Whether this node constrains nothing (an empty group). The caller maps
    /// it to "no filter" so the statement has no `WHERE` at all.
    pub fn is_vacuous(&self) -> bool {
        match self {
            FilterNode::All(children) | FilterNode::Any(children) => children.is_empty(),
            _ => false,
        }
    }

    /// How many clauses this tree holds — the number the header button shows
    /// ("Filter · 2"), counted over the whole tree so a nested document counts
    /// the way the panel's flat view would.
    pub fn clause_count(&self) -> usize {
        match self {
            FilterNode::All(children) | FilterNode::Any(children) => {
                children.iter().map(FilterNode::clause_count).sum()
            }
            FilterNode::Not(child) => child.clause_count(),
            FilterNode::Clause(_) => 1,
        }
    }

    /// The document's JSON for this node — what `ViewDefinition::set_filter`
    /// writes back (ADR-0074's read-edit-write of the text).
    pub fn to_json(&self) -> Json {
        match self {
            FilterNode::All(children) => Json::Object(vec![(
                "and".to_string(),
                Json::Array(children.iter().map(FilterNode::to_json).collect()),
            )]),
            FilterNode::Any(children) => Json::Object(vec![(
                "or".to_string(),
                Json::Array(children.iter().map(FilterNode::to_json).collect()),
            )]),
            FilterNode::Not(child) => {
                Json::Object(vec![("not".to_string(), child.to_json())])
            }
            FilterNode::Clause(clause) => Json::Object(vec![
                (
                    "property".to_string(),
                    Json::Number(clause.property.as_u64() as f64),
                ),
                (
                    "op".to_string(),
                    Json::Text(clause.op.as_str().to_string()),
                ),
                ("value".to_string(), value_json(&clause.value)),
            ]),
        }
    }
}

fn value_json(value: &FilterValue) -> Json {
    match value {
        FilterValue::Missing => Json::Null,
        FilterValue::Text(text) => Json::Text(text.clone()),
        FilterValue::Number(num) => Json::Number(*num),
        FilterValue::Flag(flag) => Json::Bool(*flag),
        FilterValue::Any(items) => {
            Json::Array(items.iter().map(|i| Json::Text(i.clone())).collect())
        }
    }
}

/// The filter subset the panel can draw and edit: one `and`/`or` root over
/// clauses, each clause optionally inverted (a `not` around a single clause).
///
/// A tree outside that subset — a group inside a group, a `not` around a group
/// — answers `None`, and the panel then **refuses to edit** rather than
/// reshaping the user's rules into something it can represent. The table still
/// filters by the tree it has (the compiler reads the whole recursive shape),
/// which is the honest split: the SQL side delivers the document ADR-0064
/// describes, and the panel delivers the subset D4 has a UI for. Nested groups
/// arrive with the board view's group editor (D5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlatFilter {
    /// `true` = the root is an `or` ("match any"), `false` = an `and`.
    pub any: bool,
    pub clauses: Vec<FlatClause>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlatClause {
    pub clause: FilterClause,
    pub invert: bool,
}

impl FlatFilter {
    /// Read the panel's view of a parsed tree. `None` = this tree is not one the
    /// panel may rewrite.
    pub fn from_tree(tree: Option<&FilterNode>) -> Option<FlatFilter> {
        let Some(tree) = tree else {
            return Some(FlatFilter::default());
        };
        match tree {
            // `All([])`/`Any([])` — the panel's own "no rules" state — come back
            // as an empty flat list of the same flavour.
            FilterNode::All(children) | FilterNode::Any(children) => {
                let any = matches!(tree, FilterNode::Any(_));
                let mut clauses = Vec::with_capacity(children.len());
                for child in children {
                    match child {
                        FilterNode::Clause(clause) => clauses.push(FlatClause {
                            clause: clause.clone(),
                            invert: false,
                        }),
                        FilterNode::Not(inner) => match &**inner {
                            FilterNode::Clause(clause) => clauses.push(FlatClause {
                                clause: clause.clone(),
                                invert: true,
                            }),
                            _ => return None,
                        },
                        _ => return None,
                    }
                }
                Some(FlatFilter { any, clauses })
            }
            FilterNode::Clause(clause) => Some(FlatFilter {
                any: false,
                clauses: vec![FlatClause {
                    clause: clause.clone(),
                    invert: false,
                }],
            }),
            FilterNode::Not(inner) => match &**inner {
                FilterNode::Clause(clause) => Some(FlatFilter {
                    any: false,
                    clauses: vec![FlatClause {
                        clause: clause.clone(),
                        invert: true,
                    }],
                }),
                _ => None,
            },
        }
    }

    /// The tree this flat list means. An empty list is `All([])`, which the
    /// parser reads back as "no filter" — so deleting the last rule and
    /// re-opening the view is the same view.
    pub fn to_tree(&self) -> FilterNode {
        let children: Vec<FilterNode> = self
            .clauses
            .iter()
            .map(|flat| {
                let clause = FilterNode::Clause(flat.clause.clone());
                if flat.invert {
                    FilterNode::Not(Box::new(clause))
                } else {
                    clause
                }
            })
            .collect();
        if self.any {
            FilterNode::Any(children)
        } else {
            FilterNode::All(children)
        }
    }
}

/// The column a grouped view groups by (SPEC §三十九's `group by`). One column,
/// not a list: ADR-0064 stores `groups` as an array so a later build can nest,
/// and D4 reads its first entry.
///
/// **Only the option-bounded kinds may group** — `checkbox`, `select`,
/// `status`. The reason is the red line itself: a group header is an entity the
/// view has to place in the scroll surface, so the list of headers has to be
/// small enough to compute in full (it is `COUNT(*) GROUP BY`, a handful of
/// rows). Grouping by `text` or by a date would make that list as long as the
/// table — 10 000 headers to realize is exactly what "group by must not become
/// 10 000 rows" forbids. A future slice can group by a number by bucketing it,
/// which is a different question (what the buckets are) and not one to guess at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSpec {
    pub property: PropertyId,
    pub kind: PropertyKind,
}

impl GroupSpec {
    /// The kinds whose distinct values a schema already bounds: a checkbox has
    /// two states, a select/status has its option list (ADR-0061) plus "no
    /// value".
    pub fn admits(kind: PropertyKind) -> bool {
        matches!(
            kind,
            PropertyKind::Checkbox | PropertyKind::Select | PropertyKind::Status
        )
    }
}

/// One group's key, normalized out of what SQL returns: the raw column value
/// for a select is an option id or nothing, and for a checkbox it is 0, 1 or
/// nothing — and "nothing" and "unchecked" are the same group, because an
/// untouched checkbox is unchecked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKey {
    /// No value: a select/status cell nobody set, or a checkbox nobody touched.
    Empty,
    /// A select/status option id — the stored value, even when the schema no
    /// longer has that option (the group header then names the id itself,
    /// ADR-0069's fold applied to a header).
    Option(String),
    Checked,
    /// `flag = 0` **and** no row at all.
    Unchecked,
}

impl GroupKey {
    /// The `GROUP BY` expression's value, normalized: SQL's `NULL` and `''` are
    /// the same group, and so are a checkbox's `0` and its absence.
    pub fn of_text(value: Option<&str>) -> GroupKey {
        match value {
            Some(text) if !text.is_empty() => GroupKey::Option(text.to_string()),
            _ => GroupKey::Empty,
        }
    }

    pub fn of_flag(value: Option<i64>) -> GroupKey {
        match value {
            Some(flag) if flag != 0 => GroupKey::Checked,
            _ => GroupKey::Unchecked,
        }
    }
}

/// One group's rows inside a window: which group, at which **entry index** the
/// slice starts (its header's position plus the skipped rows, so the caller can
/// place the fetched rows without recomputing the walk), how many of its
/// leading rows to skip, and how many to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSlice {
    pub group: usize,
    /// The entry index of the slice's first row (never the header's).
    pub at: usize,
    pub skip: usize,
    pub len: usize,
}

/// The window arithmetic for a **grouped** view, and the answer to "a group
/// header must not become a row".
///
/// The grouped view's scroll surface is a list of *entries*: for every group, a
/// header entry followed by its rows. `total_entries = Σ(count + 1)` — a number
/// computed from the group counts, which SQL produced with one `GROUP BY` over
/// an option-bounded column (never from the rows). The window is then computed
/// over that list by D0's own `core::database::window`, exactly as it is over
/// rows when nothing is grouped, and this function maps the entry window onto
/// the queries that realize it:
///
/// * `headers` — the headers that fall inside the window (usually one or two:
///   a header is one entry tall),
/// * `rows` — one slice per group that overlaps the window, with the offset
///   *inside that group*.
///
/// So a 10 000-row database grouped into three groups realizes three headers
/// and one window of rows — and a group with 10 000 rows in it realizes the
/// same 31 rows it would if it were ungrouped. **Nothing here realizes a
/// header per group**: the group list is walked to accumulate entry positions
/// (a few integers per group), which is what makes this O(groups) and not
/// O(entries).
pub fn group_window(counts: &[usize], window: RowWindow) -> GroupWindow {
    let mut out = GroupWindow {
        headers: Vec::new(),
        rows: Vec::new(),
    };
    let mut at = 0usize;
    for (group, count) in counts.iter().copied().enumerate() {
        let header = at;
        let first_row = header + 1;
        at = first_row + count;
        // Above the window: this group's entries are all behind us.
        if at <= window.start {
            continue;
        }
        // Below the window: groups are in list order, so every later one is
        // further down and the walk can stop.
        if header >= window.end {
            break;
        }
        if header >= window.start {
            out.headers.push((group, header));
        }
        let low = first_row.max(window.start);
        let high = at.min(window.end);
        if high > low {
            out.rows.push(GroupSlice {
                group,
                at: low,
                skip: low - first_row,
                len: high - low,
            });
        }
    }
    out
}

/// What one grouped window realizes: the headers on screen, and one slice of
/// rows per group that reaches into the viewport.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupWindow {
    /// `(group index, entry index)` per header inside the window. The entry
    /// index is what places it in the scroll surface.
    pub headers: Vec<(usize, usize)>,
    pub rows: Vec<GroupSlice>,
}

impl GroupWindow {
    /// Rows this window realizes — the number the RAM gate is about, and the
    /// one that must stay a viewport's worth however many rows the database
    /// has (a group header costs one more). Named `realized` rather than
    /// `rows` because `rows` is already the field of slices it sums.
    pub fn realized(&self) -> usize {
        self.rows.iter().map(|slice| slice.len).sum()
    }
}

/// A view's rules, as this build reads them out of its document. `note` is the
/// **visible degradation**: a filter the document held that this build could
/// not apply, in the words the block draws. Empty is the ordinary case.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewRules {
    pub filter: Option<FilterNode>,
    /// Most significant term first (ADR-0070's list).
    pub sorts: Vec<SortSpec>,
    pub group: Option<GroupSpec>,
    pub note: String,
}

/// The deepest filter tree this build reads. [`Json`] already refuses to parse
/// past its own depth limit, so this is a second, tighter bound for the one
/// document whose shape is recursive: a filter eight groups deep is not a
/// filter anyone wrote, and the failure is the visible one (the tree is ignored
/// and the block says so) rather than a deep recursion in the compiler.
const FILTER_MAX_DEPTH: usize = 8;

impl ViewDefinition {
    /// The document's rules, parsed against the database's schema.
    ///
    /// The schema is needed for two things and no more: which property a clause
    /// names (so a clause naming a deleted column can be dropped, ADR-0064) and
    /// which kind it is (so the comparisons a clause asks for can be checked
    /// against the ones its kind has). Reading the document without a schema
    /// would mean the compiler had to consult one per clause instead.
    pub fn rules(&self, db: DatabaseId, catalog: &DatabaseCatalog) -> ViewRules {
        let mut rules = ViewRules::default();
        let mut notes: Vec<String> = Vec::new();
        let mut dropped = 0usize;

        if let Some(filter) = self.document.get("filter") {
            match filter {
                Json::Null => {}
                json => match parse_filter(json, db, catalog, &mut dropped, 0) {
                    Err(()) => notes.push(
                        "This view's filter could not be read and was ignored.".to_string(),
                    ),
                    Ok(None) => {}
                    Ok(Some(node)) => {
                        // A group that constrains nothing is no filter at all —
                        // the state the panel leaves behind when its last rule
                        // is deleted.
                        if !node.is_vacuous() {
                            rules.filter = Some(node);
                        }
                    }
                },
            }
        }
        if dropped > 0 {
            notes.push(if dropped == 1 {
                "1 filter rule was dropped: its column is gone, or cannot be compared that way."
                    .to_string()
            } else {
                format!(
                    "{dropped} filter rules were dropped: their columns are gone, or cannot be compared that way."
                )
            });
        }

        // Sorts: terms whose column is gone, or whose kind has no order a user
        // would recognise (ADR-0070's table), are dropped quietly — an order is
        // a way of looking at rows, never a way of hiding them, so a dropped
        // term shows itself in the first frame.
        if let Some(Json::Array(terms)) = self.document.get("sorts") {
            for term in terms {
                let Some(property) = term.get("property").and_then(Json::as_u64) else {
                    continue;
                };
                let property = PropertyId(property);
                let Some(row) = catalog.properties_of(db).find(|p| p.id == property) else {
                    continue;
                };
                let descending = matches!(term.get("descending"), Some(Json::Bool(true)));
                if let Some(spec) = SortSpec::of(row, descending) {
                    rules.sorts.push(spec);
                }
            }
        }

        // One group, from the array's first entry.
        if let Some(Json::Array(groups)) = self.document.get("groups") {
            for entry in groups {
                let Some(property) = entry.as_u64() else {
                    continue;
                };
                let property = PropertyId(property);
                let Some(row) = catalog.properties_of(db).find(|p| p.id == property) else {
                    continue;
                };
                if GroupSpec::admits(row.kind) {
                    rules.group = Some(GroupSpec {
                        property,
                        kind: row.kind,
                    });
                }
                break;
            }
        }

        rules.note = notes.join(" ");
        rules
    }

    /// Replace the `filter` key with this tree (ADR-0074: one key of the
    /// document, read-edited-written as text). `None` writes JSON `null` —
    /// "this view has no filter" — rather than removing the key, so a later
    /// build reading the document sees that the key is known and empty.
    pub fn set_filter(&mut self, filter: Option<&FilterNode>) {
        self.put(
            "filter",
            match filter {
                Some(node) => node.to_json(),
                None => Json::Null,
            },
        );
    }

    /// Replace the `sorts` key with this list, most significant first.
    pub fn set_sorts(&mut self, sorts: &[SortSpec]) {
        self.put(
            "sorts",
            Json::Array(
                sorts
                    .iter()
                    .map(|sort| {
                        Json::Object(vec![
                            (
                                "property".to_string(),
                                Json::Number(sort.property.as_u64() as f64),
                            ),
                            ("descending".to_string(), Json::Bool(sort.descending)),
                        ])
                    })
                    .collect(),
            ),
        );
    }

    /// Replace the `groups` key with this one column — or with an empty array,
    /// which is "no grouping" and deliberately not the absence of the key.
    pub fn set_group(&mut self, group: Option<PropertyId>) {
        self.put(
            "groups",
            Json::Array(
                group
                    .map(|id| Json::Number(id.as_u64() as f64))
                    .into_iter()
                    .collect(),
            ),
        );
    }
}

/// One node of ADR-0064's filter tree. `Err(())` when the *skeleton* is not a
/// shape this build reads (a group whose children are not an array, or nesting
/// past [`FILTER_MAX_DEPTH`]) — the whole filter is then dropped, note and all.
/// `Ok(None)` when **this subtree** was dropped: a clause this build cannot
/// read is skipped and counted, and the tree around it survives — ADR-0064's
/// rule for a deleted property, applied to the other ways one clause can be
/// unreadable. A `not` around a dropped clause is dropped with it (a rule that
/// vanished must not come back as its own negation, hiding every row).
fn parse_filter(
    json: &Json,
    db: DatabaseId,
    catalog: &DatabaseCatalog,
    dropped: &mut usize,
    depth: usize,
) -> Result<Option<FilterNode>, ()> {
    if depth > FILTER_MAX_DEPTH {
        return Err(());
    }
    if let Some(children) = json.get("and") {
        let list = children.as_array().ok_or(())?;
        let mut out = Vec::with_capacity(list.len());
        for child in list {
            if let Some(node) = parse_filter(child, db, catalog, dropped, depth + 1)? {
                out.push(node);
            }
        }
        return Ok(Some(FilterNode::All(out)));
    }
    if let Some(children) = json.get("or") {
        let list = children.as_array().ok_or(())?;
        let mut out = Vec::with_capacity(list.len());
        for child in list {
            if let Some(node) = parse_filter(child, db, catalog, dropped, depth + 1)? {
                out.push(node);
            }
        }
        return Ok(Some(FilterNode::Any(out)));
    }
    if let Some(inner) = json.get("not") {
        return match parse_filter(inner, db, catalog, dropped, depth + 1)? {
            Some(node) => Ok(Some(FilterNode::Not(Box::new(node)))),
            // The rule under the `not` was unreadable, so the `not` goes with
            // it: a rule that vanished must not come back as its own negation.
            None => Ok(None),
        };
    }
    match build_clause(json, db, catalog) {
        Some(clause) => Ok(Some(FilterNode::Clause(clause))),
        // A clause this build cannot read: skipped, and the tree keeps its
        // other children. The count is one, because one rule disappeared from
        // the view.
        None => {
            *dropped += 1;
            Ok(None)
        }
    }
}

/// One clause, or `None` when this build cannot compare this column this way.
/// Silent on purpose — the caller counts and reports; the reason (gone column,
/// wrong comparison for the kind, value of the wrong shape) is one sentence in
/// the note because all three read the same to a user: *that rule is not being
/// applied*.
fn build_clause(json: &Json, db: DatabaseId, catalog: &DatabaseCatalog) -> Option<FilterClause> {
    let property = PropertyId(json.get("property")?.as_u64()?);
    let kind = catalog.properties_of(db).find(|p| p.id == property)?.kind;
    let op = FilterOp::try_from_str(json.get("op")?.as_str()?)?;
    if !FilterOp::ops_for(kind).contains(&op) {
        return None;
    }
    let value = if op.needs_value() {
        parse_value(json.get("value").unwrap_or(&Json::Null), kind, op)?
    } else {
        FilterValue::Missing
    };
    // A value of the wrong shape for the kind is a clause this build cannot
    // apply: `contains` on a number, a number the document wrote as a string,
    // a date that is not one of ADR-0062's two stored shapes.
    if op.needs_value() && !value.is_set() && !matches!(value, FilterValue::Missing) {
        return None;
    }
    Some(FilterClause {
        property,
        kind,
        op,
        value,
    })
}

/// The value side of a clause, in the shape its column's kind stores.
fn parse_value(json: &Json, kind: PropertyKind, op: FilterOp) -> Option<FilterValue> {
    match kind {
        PropertyKind::Number => json.as_f64().map(FilterValue::Number),
        PropertyKind::Checkbox => match json {
            Json::Bool(flag) => Some(FilterValue::Flag(*flag)),
            _ => None,
        },
        PropertyKind::Date
        | PropertyKind::CreatedTime
        | PropertyKind::LastEditedTime => match json {
            // Only the two stored shapes are comparable: bytes are time order
            // *because* the shape is fixed width (ADR-0062), so a value that is
            // not one of them would be compared against something it has
            // nothing to do with.
            Json::Text(text) if is_stored_date(text) => Some(FilterValue::Text(text.clone())),
            Json::Text(text) if text.is_empty() => Some(FilterValue::Missing),
            _ => None,
        },
        PropertyKind::Select
        | PropertyKind::Status
        | PropertyKind::MultiSelect
        | PropertyKind::Files => {
            if op == FilterOp::AnyOf {
                parse_list(json)
            } else {
                match json {
                    Json::Text(text) if text.is_empty() => Some(FilterValue::Missing),
                    Json::Text(text) => Some(FilterValue::Text(text.clone())),
                    _ => None,
                }
            }
        }
        // Text-shaped kinds: url / email / phone are never rewritten (ADR-0069)
        // and neither is title/text, so the filter value is compared verbatim.
        _ => match json {
            Json::Text(text) if text.is_empty() => Some(FilterValue::Missing),
            Json::Text(text) => Some(FilterValue::Text(text.clone())),
            _ => None,
        },
    }
}

/// An "any of" value: the document's array, or one bare string, which is the
/// same rule with one option in it. An empty array is the rule nobody has
/// picked anything for yet — [`FilterValue::Missing`], no constraint, so
/// "add a rule" can be left half-done without hiding rows.
fn parse_list(json: &Json) -> Option<FilterValue> {
    match json {
        Json::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(item.as_str()?.to_string());
            }
            if out.is_empty() {
                Some(FilterValue::Missing)
            } else {
                Some(FilterValue::Any(out))
            }
        }
        Json::Text(text) if !text.is_empty() => Some(FilterValue::Any(vec![text.clone()])),
        Json::Text(_) => Some(FilterValue::Missing),
        _ => None,
    }
}

/// Whether `text` is one of ADR-0062's two stored date shapes —
/// `YYYY-MM-DDTHH:MM` or `YYYY-MM-DD` — the only values a date-ish filter may
/// compare.
///
/// A **shape** check and not a calendar one: `database_property::parse_one`
/// already owns calendar validity for cell input, and a hand-edited
/// `2026-13-99` reaching a filter would simply match nothing, which is visible
/// and harmless. Refusing shapes is what keeps a stray `next tuesday` out of a
/// comparison whose whole correctness rests on fixed width.
pub fn is_stored_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits_at = |at: usize| bytes.get(at).map(u8::is_ascii_digit).unwrap_or(false);
    let date_ok = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (0..10).all(|at| at == 4 || at == 7 || digits_at(at));
    if bytes.len() == 10 {
        return date_ok;
    }
    bytes.len() == 16
        && date_ok
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && (11..16).all(|at| at == 13 || digits_at(at))
}

// ─── D5: the view family (board / list / calendar / gallery / timeline / form) ─
//
// D3 drew the table and D4 gave a view its rules; D5 adds the six SPEC layouts
// that come after it (chart is D7's). ADR-0060 made all eight **one entity's
// layouts** rather than eight block kinds, so every view below is a `layout`
// string plus a projection — the block, the switcher, the window arithmetic and
// the document are all the same objects the table already used. What differs per
// layout is only:
//
//   1. **what the window is opened on.** SPEC's red line is
//      「10 000 行不得全量 realize；视图先算可见窗口再取行」, and "先算窗口" is
//      per-shape arithmetic:
//        * table / list / timeline — a window of *rows* (D0's `window`),
//        * board — a window of *card slots*: one slot is a horizontal band
//          across every column, and each group fetches its own slice of that
//          band (`board_window` below). The column *headers* are the group
//          list from one `GROUP BY` over an option-bounded column — a handful
//          of rows, never one header per card,
//        * gallery — a window of card *rows* (`per_row` cards each), fetched as
//          one slice of `per_row × rows` cards,
//        * calendar — the month grid is fixed (6×7), so what is windowed is
//          the records *inside* a day: one `GROUP BY` over the date column for
//          the month's counts (≤ 31 rows) and at most [`CALENDAR_PEEK`]
//          records per day, with the rest folded into a count,
//        * form — nothing to window: it is the *field list*, bounded by the
//          schema, and it creates rows rather than reading them.
//   2. **how a row is painted.** The store paints every visible cell for all
//      layouts (that is D2's pipeline, unchanged); a layout's delegate picks
//      the ones it shows. A list row reads the first two cells as its preview,
//      a card reads the first checkbox cell for its box, a timeline lane reads
//      the two date columns' day numbers (computed here, in Rust, because a
//      delegate may not parse a date — hard rule).
//
// Everything in this section is **pure**: no SQL, no Slint, no clock (the
// module's D0 contract). Calendar arithmetic is written out here rather than
// borrowed from `core::date` because that module is another track's, and a
// dependency on an uncommitted file is exactly what D1's `E0583` taught the
// track not to do. The algorithms are the standard civil-date ones and carry
// their own comments.

/// One layout's own geometry, in px — the one source the window arithmetic and
/// the delegate both read. `header_height` is the same 64 px for every layout
/// (the switcher line plus the toolbar line); `row_height` is the height of one
/// *placement unit*: a table row, a list row, a timeline lane — and, for the
/// layouts that place cards rather than rows, the card slot the window counts
/// in ([`TableView::BOARD_CARD_HEIGHT`] / [`TableView::GALLERY_CARD_HEIGHT`]).
///
/// The reason this is a function and not eight constants read at eight call
/// sites: `core::database::window` divides the scroll offset by the row height,
/// so a delegate laid out at one height while the window arithmetic used
/// another would fetch a window that does not cover the viewport — the defect
/// D3's `db-row-height = 0` bug was, in another form.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutMetrics {
    pub row_height: f32,
    pub header_height: f32,
}

pub fn layout_metrics(layout: ViewLayout) -> LayoutMetrics {
    let row_height = match layout {
        ViewLayout::Table => TableView::ROW_HEIGHT,
        ViewLayout::List => TableView::LIST_ROW_HEIGHT,
        ViewLayout::Timeline => TableView::TIMELINE_ROW_HEIGHT,
        ViewLayout::Board => TableView::BOARD_CARD_HEIGHT,
        ViewLayout::Gallery => TableView::GALLERY_CARD_HEIGHT,
        // The calendar's grid, the form's fields and the chart's plot are not
        // windows over a placement unit at all; the numbers are here so a
        // caller that asks gets the table's own row rather than a zero to
        // divide by.
        ViewLayout::Calendar | ViewLayout::Form | ViewLayout::Chart => {
            TableView::ROW_HEIGHT
        }
    };
    LayoutMetrics {
        row_height,
        header_height: TableView::HEADER_HEIGHT,
    }
}

impl TableView {
    /// A list row: the title on its own line and the first two columns as a
    /// muted preview line under it. Taller than a table row because it *is*
    /// two lines of text — and the number lives here, once, because the window
    /// arithmetic divides by it.
    pub const LIST_ROW_HEIGHT: f32 = 44.0;
    /// A board card: the title plus one checkbox row. Fixed so a column of
    /// cards is a grid rather than a scale.
    pub const BOARD_CARD_HEIGHT: f32 = 76.0;
    /// How tall a board column's header is (the option's name and its count) —
    /// **not** part of the window: a header costs one small rectangle however
    /// many cards its column holds ([`group_window`]'s contract, restated for
    /// a horizontal layout).
    pub const BOARD_HEADER_HEIGHT: f32 = 28.0;
    /// A gallery card: an avatar band plus two lines. The cover *image* is a
    /// files column's first attachment and is D8's (the attachment thumbnail
    /// path); D5 draws the letter avatar, which the brief allows as the
    /// fallback and which needs no decode per realized card.
    pub const GALLERY_CARD_HEIGHT: f32 = 132.0;
    /// The narrowest a gallery card may be before the layout drops a column
    /// instead of squeezing it. The delegate's `per-row` formula and this
    /// number have to agree or the reported shape would fight the drawing.
    pub const GALLERY_CARD_MIN_WIDTH: f32 = 168.0;
    /// One timeline lane: the bar's own band.
    pub const TIMELINE_ROW_HEIGHT: f32 = 36.0;
    /// The calendar's month strip (‹ September 2026 ›), the weekday initials
    /// row, and one week. The grid is **fixed** — six weeks is what a month
    /// can need — so the calendar's surface height is a constant and the
    /// virtualization inside it is per-day (see [`CALENDAR_PEEK`]).
    pub const CALENDAR_NAV_HEIGHT: f32 = 28.0;
    pub const CALENDAR_WEEKDAY_HEIGHT: f32 = 20.0;
    pub const CALENDAR_WEEK_HEIGHT: f32 = 96.0;
    /// One form field (label + editor) and the form's action row.
    pub const FORM_FIELD_HEIGHT: f32 = 40.0;
    pub const FORM_ACTIONS_HEIGHT: f32 = 48.0;
    /// The chart's surface below the header, and the legend strip under the
    /// plot inside it. A constant, like the calendar's grid: the plot is
    /// **aggregates**, not rows, so the surface does not grow with the data —
    /// what grows with the data is the *numbers inside the shapes*, and those
    /// are scalars SQL produced (see the chart branch's contract).
    pub const CHART_HEIGHT: f32 = 260.0;
    /// The label strip the three shapes draw their group labels in.
    pub const CHART_LABEL_HEIGHT: f32 = 24.0;

    /// The surface below the header for the layouts that place one unit per
    /// counted item — a table row, a list row, a timeline lane, a board slot.
    pub fn rows_surface_height(row_height: f32, total: usize) -> f32 {
        row_height * total as f32
    }

    /// A gallery's surface: `ceil(total / per_row)` card rows. The rows are the
    /// window's unit; the cards inside one row come back in a single slice.
    pub fn gallery_surface_height(total: usize, per_row: usize) -> f32 {
        Self::GALLERY_CARD_HEIGHT * Self::gallery_rows(total, per_row) as f32
    }

    /// How many card rows a gallery has, with `per_row >= 1` forced: the
    /// formula the delegate's reported `per-row` is the input to, so the two
    /// halves of "how tall is the grid" cannot disagree.
    pub fn gallery_rows(total: usize, per_row: usize) -> usize {
        let per_row = per_row.max(1);
        (total + per_row - 1) / per_row
    }

    /// How many cards fit in one row at this grid width — the delegate's own
    /// formula, restated here so a seed or a default computes the same number
    /// the delegate would report. A grid narrower than one card still shows
    /// one column (a card too narrow to read is still better than no card).
    pub fn gallery_per_row(grid_width: f32) -> usize {
        if grid_width <= 0.0 {
            return 1;
        }
        ((grid_width / Self::GALLERY_CARD_MIN_WIDTH).floor() as usize).max(1)
    }

    /// The calendar's surface: the month strip, the weekday initials, and six
    /// weeks — a constant, because the grid does not grow with the data. This
    /// is the whole difference between the calendar and every other layout: the
    /// red line is about **rows**, and a month has at most 42 cells.
    pub fn calendar_surface_height() -> f32 {
        Self::CALENDAR_NAV_HEIGHT
            + Self::CALENDAR_WEEKDAY_HEIGHT
            + CALENDAR_WEEKS as f32 * Self::CALENDAR_WEEK_HEIGHT
    }

    /// A form's surface: one field per visible column plus the action row. No
    /// records are read to draw it, which is why a form is the one layout that
    /// cannot be asked to realize a row it does not have.
    pub fn form_surface_height(fields: usize) -> f32 {
        fields as f32 * Self::FORM_FIELD_HEIGHT + Self::FORM_ACTIONS_HEIGHT
    }

    /// The chart's surface: plot plus label strip, a constant. The chart is
    /// the second layout (after the form) whose body never grows with the
    /// data — the red line is about row *objects*, and a chart realizes none.
    pub fn chart_surface_height() -> f32 {
        Self::CHART_HEIGHT
    }
}

/// The calendar's fixed grid: six weeks of seven days, so a month that starts
/// on a Sunday in a 31-day month still fits.
pub const CALENDAR_WEEKS: usize = 6;
pub const CALENDAR_COLUMNS: usize = 7;
/// How many records a day cell draws before it folds the rest into a count.
/// Three is what fits in one cell at [`TableView::CALENDAR_WEEK_HEIGHT`]; the
/// count is not decoration but the *virtualization*: a day with 500 records
/// realizes three, and the cell says "and 497 more".
pub const CALENDAR_PEEK: usize = 3;

/// The board's answer to "a group header must not become a row", in the
/// horizontal direction.
///
/// A board's scroll surface is a stack of *card slots*: slot `s` is one
/// horizontal band across every column, and the board is
/// `max(column counts)` slots tall. The window is computed over those slots by
/// D0's own `window` (the caller passes `max(counts)` as the total), and this
/// function maps it onto the per-column queries that realize it: for each
/// column, how many of its leading cards to skip and how many to take.
///
/// So the objects that exist are `Σ len` ≤ `columns × (visible slots +
/// overscan)` — bounded by the *viewport and the group list*, not by the table.
/// A column holding 10 000 cards realizes the same handful of cards the
/// ungrouped table would; a board with three columns realizes three slices, and
/// its column headers are the group list (a handful of rows from one
/// `GROUP BY`), never one per card.
///
/// Cards are placed at `slot × card_height` inside their column, so every
/// realized card's slot is `window.start + model_index` — one expression in the
/// delegate, the same row→slot conversion §三十七 requires of a row-based
/// block.
pub fn board_window(counts: &[usize], window: RowWindow) -> Vec<(usize, usize, usize)> {
    counts
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(group, count)| {
            let skip = window.start.min(count);
            let len = window.len().min(count.saturating_sub(skip));
            if len == 0 {
                None
            } else {
                Some((group, skip, len))
            }
        })
        .collect()
}

/// How many card slots a board's surface has: the tallest column. Zero columns
/// (a board with no grouping yet) is zero slots, which is what the empty state
/// draws.
pub fn board_slots(counts: &[usize]) -> usize {
    counts.iter().copied().max().unwrap_or(0)
}

/// The days of a month, 1-based — the calendar's own arithmetic. Leap years
/// follow the Gregorian rule (a year divisible by 4, except centuries not
/// divisible by 400), which is what the stored `YYYY-MM-DD` values of a
/// hand-edited file are read against.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days since 1970-01-01 for a civil date — Howard Hinnant's `days_from_civil`
/// with the era arithmetic kept explicit. It is here rather than in a crate
/// because the app has no date library and must not grow one for a bar's `x`
/// (the same argument ADR-0022 made about Markdown), and it is a *day number*
/// rather than a `Date`: the only questions asked of it are "which is earlier"
/// and "how many days apart".
///
/// Out-of-range fields are not invalidated (a hand-edited `2026-13-99` maps to
/// a day number none of its neighbours share) for the same reason
/// [`is_stored_date`] checks the shape and not the calendar: a wrong bar is
/// visible and harmless, a refused read is not.
pub fn day_number(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let m = month as i64;
    let d = day as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The day number of a **stored (or painted) date value**, or `None` when the
/// text is not one of ADR-0062's shapes. A painted date cell is
/// `DateFormat::Date`'s first ten characters or the full stored text, and both
/// start `YYYY-MM-DD`, so the first ten characters are the day — which is the
/// one place the timeline and the calendar read a date out of a cell, and the
/// reason neither needs a second query per row.
pub fn day_number_of(text: &str) -> Option<i64> {
    if !is_stored_date(text) {
        return None;
    }
    let year = text.get(..4)?.parse::<i32>().ok()?;
    let month = text.get(5..7)?.parse::<u32>().ok()?;
    let day = text.get(8..10)?.parse::<u32>().ok()?;
    Some(day_number(year, month, day))
}

/// The Monday-based weekday of a month's first day, `0 = Monday … 6 = Sunday`.
/// 1970-01-01 was a Thursday, which is day number 0, so the offset is three.
/// A calendar week that starts on Monday is the Notion shape the brief asks
/// for, and the expression is the whole of the choice.
pub fn first_weekday_monday0(year: i32, month: u32) -> u32 {
    let day = day_number(year, month, 1);
    ((day + 3).rem_euclid(7)) as u32
}

/// `2004` → `"2004-09-22"`. The stored date shape, written where the calendar
/// builds the range clauses of a month (ADR-0062: bytes are time order, which
/// is what makes `>=`/`<` a date comparison in SQL).
pub fn date_key(year: i32, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// `"2004-09"` — the month's first day as a key, which is the lower bound of
/// the month's range clause.
pub fn month_key(year: i32, month: u32) -> String {
    date_key(year, month, 1)
}

/// The month a stored date value falls in, or `None` when it is not a date.
pub fn month_of(text: &str) -> Option<(i32, u32)> {
    if !is_stored_date(text) {
        return None;
    }
    let year = text.get(..4)?.parse::<i32>().ok()?;
    let month = text.get(5..7)?.parse::<u32>().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month))
}

/// The day of the month a stored date value names (`"2026-09-22T10:00"` → 22),
/// or `None` when it is not a date — the key the calendar's per-day queries are
/// built from.
pub fn day_of(text: &str) -> Option<u32> {
    month_of(text)?;
    text.get(8..10)?.parse::<u32>().ok()
}

/// Step a month by `delta` (a day's worth of intuition: `-1` is the month
/// before, `+1` the one after), with the year carried. Written out rather than
/// with `%`/`/` on a signed month because a calendar jumps from January
/// backwards to December and the sign has to come out right.
pub fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let mut y = year as i64;
    let mut m = month as i64 + delta as i64;
    while m < 1 {
        m += 12;
        y -= 1;
    }
    while m > 12 {
        m -= 12;
        y += 1;
    }
    (y as i32, m as u32)
}

/// The English month names the calendar's strip draws. The app's UI language is
/// English (every other label in this build is), and a name table is the whole
/// of what an i18n layer would change here.
pub const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `(2026, 9)` → `"September 2026"` — the calendar's strip label.
pub fn month_label(year: i32, month: u32) -> String {
    let name = MONTH_NAMES
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("");
    format!("{name} {year}")
}

/// One month's grid: `CALENDAR_WEEKS * CALENDAR_COLUMNS` cells, leading and
/// trailing days zero. `0` is a blank cell (the delegate draws nothing there);
/// `1..=31` is a day of `year`/`month`.
///
/// The grid is a *constant* size so that a month's shape does not depend on
/// which month it is — the same reason the surface height is a constant. A
/// hand-edited date outside the month's days (`2026-02-30`) composes a day
/// cell that never matches a query, which is where the fold and the count keep
/// it from lying about the grid.
pub fn month_cells(year: i32, month: u32) -> Vec<i32> {
    let lead = first_weekday_monday0(year, month) as usize;
    let days = days_in_month(year, month) as i32;
    let mut cells = vec![0i32; CALENDAR_WEEKS * CALENDAR_COLUMNS];
    for day in 1..=days {
        let at = lead + (day as usize - 1);
        if at < cells.len() {
            cells[at] = day;
        }
    }
    cells
}

// ─── D5's document keys ──────────────────────────────────────────────────────
//
// ADR-0064's one JSON document per view keeps growing by keys, never by tables
// (ADR-0064/0074), and D5 owns **one** of them: `date` (plus its optional
// partner `end`), which names the column a calendar places records by and a
// timeline draws bars from.
//
// Why one key serves two layouts: both ask the same question — "which column is
// this view's time axis" — and a second key would let the two answers drift
// while meaning the same thing. What differs is only the fallback: a calendar
// with no key falls back to the first date-kind column, because a calendar with
// no time axis is a grid of nothing. A `sorts`/`filter` key is *not* reused for
// this: those change membership and order, and neither says "which day".

impl ViewDefinition {
    /// The column this view's time axis is, as stored (`None` when the view has
    /// no opinion or the key is not an id). Whether the column still exists, and
    /// which kind it is, is the caller's check — a document is read by the layer
    /// that has the schema.
    pub fn date_column(&self) -> Option<PropertyId> {
        self.document.get("date").and_then(Json::as_u64).map(PropertyId)
    }

    /// Write the time axis (`None` stores JSON `null`, "known and empty" — the
    /// same rule `set_filter` follows, so a later build can tell a view that
    /// never had the key from one whose column was cleared).
    pub fn set_date(&mut self, property: Option<PropertyId>) {
        self.put(
            "date",
            match property {
                Some(id) => Json::Number(id.as_u64() as f64),
                None => Json::Null,
            },
        );
    }

    /// The optional **end** column: a timeline bar spans from `date` to `end`
    /// when the record has both, and is a point when it does not (the brief's
    /// 「起=止=同一天时画点」). Optional because most databases have one date.
    pub fn end_column(&self) -> Option<PropertyId> {
        self.document.get("end").and_then(Json::as_u64).map(PropertyId)
    }

    pub fn set_end(&mut self, property: Option<PropertyId>) {
        self.put(
            "end",
            match property {
                Some(id) => Json::Number(id.as_u64() as f64),
                None => Json::Null,
            },
        );
    }

    /// The chart shape this view draws (`"bar"` / `"line"` / `"pie"`), read
    /// out of the view's own document — D7's one key of ADR-0064's, written
    /// and read under the same ADR-0074 discipline as `filter`/`sorts`/`groups`
    /// and D5's `date`/`end`: a view's *shape* is a key of its document, never
    /// a second table. Anything the word does not name is a bar, which is the
    /// fold every unknown setting takes (an unparsable chart still opens).
    pub fn chart_kind(&self) -> ChartKind {
        self.document
            .get("chart")
            .and_then(Json::as_str)
            .and_then(ChartKind::try_from_str)
            .unwrap_or(ChartKind::Bar)
    }

    /// Store the chart shape (`None` = the key's absence, which reads as bar).
    /// Unlike `set_filter`'s "known and empty" null, absence is honest here:
    /// bar is both the default and the value a fresh chart means, so writing
    /// the word adds a key without adding information.
    pub fn set_chart_kind(&mut self, kind: ChartKind) {
        self.put("chart", Json::Text(kind.as_str().to_string()));
    }
}

/// The three shapes a chart view draws (SPEC §三十九 「视图」: bar / line / pie,
/// from the existing primitives — no chart library). The int is the index the
/// switcher's three buttons send, the same rule `FilterOp`'s list follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
}

pub const CHART_KINDS: [ChartKind; 3] = [ChartKind::Bar, ChartKind::Line, ChartKind::Pie];

impl ChartKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChartKind::Bar => "bar",
            ChartKind::Line => "line",
            ChartKind::Pie => "pie",
        }
    }

    pub fn try_from_str(s: &str) -> Option<ChartKind> {
        CHART_KINDS.iter().copied().find(|k| k.as_str() == s)
    }

    /// The index the chart body's three buttons send and `db-chart-kind`
    /// receives — position in [`CHART_KINDS`], never a hand-picked number.
    pub fn index(self) -> usize {
        CHART_KINDS.iter().position(|k| *k == self).unwrap_or(0)
    }

    pub fn from_index(at: usize) -> ChartKind {
        CHART_KINDS.get(at).copied().unwrap_or(ChartKind::Bar)
    }

    /// The button's word.
    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Bar => "Bar",
            ChartKind::Line => "Line",
            ChartKind::Pie => "Pie",
        }
    }
}

// ─── D7: the chart's geometry (pure, like everything else in this file) ──────
//
// The chart is the third layout (after the form and the calendar's grid) whose
// surface is a **constant** and whose data is **aggregates**: the plot is the
// view's group list — the same handful of `(key, count)` rows one `GROUP BY`
// over an option-bounded column produces for the board's columns and the
// grouped table's headers — and not one object per record, ever. A chart of a
// 10 000-row database realizes **zero rows** and draws `counts.len()` shapes;
// the red line (「10 000 行不得全量 realize」) is met by construction, the way
// the calendar's fold meets it.
//
// What is computed *here* (pure, once per refresh) and what the delegate does
// with it:
//
// * **bar** — the delegate places `counts.len()` equal-width rectangles; the
//   height of each is `frac * plot-height`, a multiply. No path needed.
// * **line** — one polyline through the points, as a path in a fixed 100×100
//   viewbox ([`chart_line_path`]). The `viewbox` scales it to the plot's real
//   size, so Rust needs no pixel width and the delegate needs no arithmetic
//   beyond placing the path and its dots (dots at the same fractions).
// * **pie** — one filled path **per slice**, again in the 100×100 viewbox
//   ([`chart_pie_paths`]). The arcs are cubic Béziers (the κ ≈ 0.5523
//   approximation, one segment per ≤ 90° of arc) rather than SVG `A`
//   commands: the numbers are machine-built here, where trig lives, and the
//   delegate draws a string it cannot get wrong.
//
// The fractions come from the counts, and the counts come from SQL — nothing
// in this section reads a row, and nothing in the delegate parses a string.

/// The one polyline of a line chart, as a path in a 100×100 viewbox: point `i`
/// sits at `x = (i + ½) · 100 / n` (centred in its slot, the way a bar is) and
/// `y = 98 − frac · 96`, so `frac = 1.0` nearly touches the top and `0.0` the
/// bottom, with a 2-unit margin each way. One point draws a bare move (the
/// delegate's dot carries it); none draws nothing.
pub fn chart_line_path(fracs: &[f64]) -> String {
    if fracs.is_empty() {
        return String::new();
    }
    let step = 100.0 / fracs.len() as f64;
    let at = |i: usize| ((i as f64 + 0.5) * step, 98.0 - fracs[i] * 96.0);
    let (x0, y0) = at(0);
    let mut out = format!("M {x0:.2} {y0:.2}");
    for i in 1..fracs.len() {
        let (x, y) = at(i);
        out.push_str(&format!(" L {x:.2} {y:.2}"));
    }
    out
}

/// The π/2 cubic-Bézier control distance (κ = 4/3 · (√2 − 1)): the constant
/// that makes one cubic per quarter-circle the standard arc approximation.
const ARC_KAPPA: f64 = 0.552_284_749_830_793_3;

/// One pie slice as a filled path in a 100×100 viewbox: centre (50, 50),
/// radius 47, starting at 12 o'clock and running clockwise in `fracs` order.
/// Each slice is `M` at the centre, `L` to its arc's start, one cubic per
/// ≤ 90° of arc, `Z` — the shape the delegate fills with the point's colour.
///
/// A slice with a zero fraction gets an empty string (nothing to fill; the
/// label still draws). A **single** slice that spans the whole circle gets the
/// special-cased full disk — four cubics and no centre line, because
/// `M c L p … Z` of a 360° arc degenerates to nothing.
pub fn chart_pie_paths(fracs: &[f64]) -> Vec<String> {
    let total: f64 = fracs.iter().sum();
    if fracs.is_empty() || total <= 0.0 {
        return vec![String::new(); fracs.len()];
    }
    // A full disk: the one slice whose arc is the whole circle (a `total > 0`
    // single-count chart — the degenerate pie is still a value, so it draws
    // the disk rather than nothing). Four cubics of exactly 90° each, starting
    // at the top and running clockwise.
    if fracs.len() == 1 {
        let rad = 47.0f64;
        let k = ARC_KAPPA * rad;
        let pt = |dx: f64, dy: f64| format!("{:.2} {:.2}", 50.0 + dx, 50.0 + dy);
        return vec![format!(
            "M {top} C {a} {b} {right} C {c} {d} {bottom} C {e} {f} {left} C {g} {h} {top} Z",
            top = pt(0.0, -rad),
            a = pt(k, -rad),
            b = pt(rad, -k),
            right = pt(rad, 0.0),
            c = pt(rad, k),
            d = pt(k, rad),
            bottom = pt(0.0, rad),
            e = pt(-k, rad),
            f = pt(-rad, k),
            left = pt(-rad, 0.0),
            g = pt(-rad, -k),
            h = pt(-k, -rad),
        )];
    }
    let radius = 47.0;
    let mut out = Vec::with_capacity(fracs.len());
    let mut angle = -std::f64::consts::FRAC_PI_2; // 12 o'clock
    for frac in fracs {
        let sweep = frac / total * std::f64::consts::TAU;
        if sweep <= f64::EPSILON {
            out.push(String::new());
            continue;
        }
        // The slice's edge, split into ≤ 90° segments so each cubic stays in
        // the κ approximation's accuracy band.
        let segments = ((sweep / (std::f64::consts::FRAC_PI_2)).ceil() as usize).max(1);
        let step = sweep / segments as f64;
        let point = |a: f64| (50.0 + radius * a.cos(), 50.0 + radius * a.sin());
        let (sx, sy) = point(angle);
        let mut path = format!("M 50 50 L {sx:.2} {sy:.2}");
        let mut a = angle;
        for _ in 0..segments {
            let (x0, y0) = point(a);
            let (x1, y1) = point(a + step);
            // Control points: ±κ·r·tan(step/2) along the tangents at each end
            // — the general form of the quarter-circle κ rule.
            let t = ARC_KAPPA * (step / 2.0).tan() * radius;
            let c1 = (x0 - t * a.sin(), y0 + t * a.cos());
            let c2 = (x1 + t * (a + step).sin(), y1 - t * (a + step).cos());
            path.push_str(&format!(
                " C {:.2} {:.2} {:.2} {:.2} {x1:.2} {y1:.2}",
                c1.0, c1.1, c2.0, c2.1
            ));
            a += step;
        }
        path.push_str(" Z");
        out.push(path);
        angle += sweep;
    }
    out
}

