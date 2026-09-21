// Database (SPEC §三十九) — the thin end of the object model, and the one
// thing this track proves before anything is drawn: a view realizes the rows
// its viewport can show, and the rows it does not show never become objects.
//
// D0 (2026-09-22) ships the projection and nothing else. The tables, the typed
// property model and the views themselves are D1–D5, and their shape is
// written down in ADR-0060…ADR-0065. What is here is the part of the SPEC's
// first red line that can be measured today:
//
//     10 000 行的库不得全量 realize；视图先算可见窗口再取行
//
// It is pure: no SQL, no Slint, no clock. The row count comes from the caller
// (`COUNT(*)`), the rows come from the caller's fetch of exactly the window
// `window()` computed, and `RowWindow::fetch` is the pair a query takes
// (`LIMIT`/`OFFSET`) — so the rows in memory are the rows the query returned by
// construction rather than by discipline. That is the same argument ADR-0028
// makes for a folded subtree one level down, and the same one ADR-0031 makes
// for a grid: the hidden thing has no representation at all.
//
// D1 adds the object model and the value shape below (`Database`, `Property`,
// `Record`, `View`, `CellValue`) — data only, still no SQL: the storage layer
// (`storage::database_store`) reads and writes it, and `RowRequest` is what it
// needs to run a windowed read. ADR-0066/0067 are where D1's two new decisions
// are written down.
//
// D2 adds the property system's two halves and neither is in this file: *what a
// cell means* is `core::database_property` (the parse and paint rules, the
// option list, the one JSON reader), and *what a sort asks SQL for* is
// `SortSpec` below — the compiled form of "order by this column", which the
// store turns into an `ORDER BY` (ADR-0069). What changes here is what the
// fourteen kinds *are*: the two derived time kinds stop pretending to be text
// values (ADR-0068), and `RowRequest` grows the sort the view will compile.

use super::types::{OrderKey, PageId};

/// Extra rows kept realized above and below the visible band, so a scroll of
/// one row does not immediately need a fetch. In rows and not in pixels,
/// because what a window costs is the number of row objects it holds; eight is
/// about half a screen at the grid's row height, and D3 re-measures it.
pub const DEFAULT_OVERSCAN: usize = 8;

/// Row height to assume when a row has not been measured yet — Slint reports a
/// zero height for the first layout pass. Dividing by it would make the window
/// infinite, so the projection floors at 1 px: one frame over-realizes rather
/// than dividing by zero, and nothing is ever lost by it.
const MIN_ROW_HEIGHT: f32 = 1.0;

/// The scroll surface of one view, in the units Slint hands the app: a row is
/// as tall as the last row it measured, and the viewport is the height of the
/// list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewGeometry {
    pub row_height: f32,
    pub viewport_height: f32,
    /// Rows kept outside the visible band; see [`DEFAULT_OVERSCAN`].
    pub overscan: usize,
}

impl ViewGeometry {
    /// The ordinary case: the measured row height, the live viewport, and the
    /// standard overscan. A caller with a reason to over-realize says so.
    pub fn new(row_height: f32, viewport_height: f32) -> Self {
        Self {
            row_height,
            viewport_height,
            overscan: DEFAULT_OVERSCAN,
        }
    }

    /// The height one row is laid out at, with the unmeasured case folded in.
    fn step(&self) -> f32 {
        if self.row_height.is_finite() && self.row_height >= MIN_ROW_HEIGHT {
            self.row_height
        } else {
            MIN_ROW_HEIGHT
        }
    }

    /// Rows the viewport itself shows. Never zero: a viewport shorter than one
    /// row still shows one row, and a window of nothing is a view that cannot
    /// be scrolled into anything.
    fn visible_rows(&self) -> usize {
        let height = if self.viewport_height.is_finite() {
            self.viewport_height.max(0.0)
        } else {
            0.0
        };
        ((height / self.step()).ceil() as usize).max(1)
    }
}

/// Which rows of a view exist as objects at all, for one scroll position.
/// `end` is exclusive, so `len()` is the number of realized rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowWindow {
    pub start: usize,
    pub end: usize,
}

impl RowWindow {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, row: usize) -> bool {
        row >= self.start && row < self.end
    }

    /// The fetch plan: the `LIMIT`/`OFFSET` a windowed read runs, and the
    /// number of rows the caller is allowed to hand back.
    pub fn fetch(&self) -> (usize, usize) {
        (self.len(), self.start)
    }
}

/// The rows `total` of a view realize at scroll offset `scroll_y`, in pixels
/// from the top of the list.
pub fn window(total: usize, geometry: ViewGeometry, scroll_y: f32) -> RowWindow {
    if total == 0 {
        return RowWindow { start: 0, end: 0 };
    }
    // The offset is clamped to the content the way Slint clamps it. A table
    // that shrank under an offset still in flight, or one that never could
    // scroll (shorter than its viewport), must not compute a window near its
    // end while the visible band still starts at row 0: the window has to cover
    // what the viewport shows, and this is the only place that knows it.
    let offset = scroll_y.max(0.0).min(max_scroll_y(total, geometry));
    let first = (offset / geometry.step()) as usize;
    let end = first
        .saturating_add(geometry.visible_rows())
        .saturating_add(geometry.overscan)
        .min(total);
    RowWindow {
        start: first.saturating_sub(geometry.overscan),
        end,
    }
}

/// Furthest the viewport can scroll before it shows the table's last row. A
/// window computed from this offset is a whole screenful; a window computed
/// from a larger one is the tail of the table.
pub fn max_scroll_y(total: usize, geometry: ViewGeometry) -> f32 {
    let content = total as f32 * geometry.step();
    (content - geometry.viewport_height.max(0.0)).max(0.0)
}

/// One row as the grid paints it: the identity, the first column, and the cells
/// of the properties this view makes visible — as text, because the *stored*
/// value is typed (ADR-0062) and this is the painted form of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub record: u64,
    pub title: String,
    pub cells: Vec<String>,
}

/// The realized slice of one view: how many rows the table has, which window of
/// them exists, and exactly those rows. `rows.len() <= window.len()` is
/// maintained by the only constructor, so no code path can hold the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealizedRows {
    total: usize,
    window: RowWindow,
    rows: Vec<RowView>,
}

impl RealizedRows {
    /// Scroll to `scroll_y`: compute the window, then ask `fetch` for exactly
    /// those rows. The red line's sentence is this function — the window is
    /// computed *before* any row is asked for, and the fetch is handed nothing
    /// but the window.
    pub fn scroll_to(
        total: usize,
        geometry: ViewGeometry,
        scroll_y: f32,
        fetch: impl FnOnce(RowWindow) -> Vec<RowView>,
    ) -> Self {
        let window = window(total, geometry, scroll_y);
        let mut rows = fetch(window);
        // A repository that hands back more than the window asked for is a bug,
        // and one that hands back fewer is a short read. Neither may become a
        // model whose length disagrees with the window it claims to show.
        rows.truncate(window.len());
        Self {
            total,
            window,
            rows,
        }
    }

    /// Rows the table has, as the caller's `COUNT(*)` reported them.
    pub fn total(&self) -> usize {
        self.total
    }

    pub fn window(&self) -> RowWindow {
        self.window
    }

    pub fn rows(&self) -> &[RowView] {
        &self.rows
    }

    /// Rows that exist as objects — the number the RAM gate is about, and the
    /// only number in this module that is not the caller's.
    pub fn realized(&self) -> usize {
        self.rows.len()
    }

    /// What the fetch that produced these rows ran.
    pub fn fetch(&self) -> (usize, usize) {
        self.window.fetch()
    }
}

// ─── D1: the object model ───────────────────────────────────────────────────
//
// SPEC §三十九's four entities — the database, its properties, its records and
// its views — plus the shape a cell's value has (ADR-0062). Data only, like
// `core::types`: no SQL, no Slint, no clock, and no derived copy of anything
// the store already knows (ADR-0039).

/// A stable id, newtyped — SPEC §九's rule that an id is an id and never an
/// index. The macro is a local copy of `core::types`'s `id_newtype!` rather
/// than an export of it: that macro is private to its module, and this slice's
/// standing instruction is to append to the shared files, not to reorganise
/// them.
macro_rules! db_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl $name {
            pub fn as_u64(self) -> u64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

db_id!(DatabaseId, "Stable identifier of a database entity (ADR-0060).");
db_id!(
    PropertyId,
    "Stable identifier of a property — one column of one database (ADR-0061)."
);
db_id!(
    RecordId,
    "Stable identifier of a record — one row of one database (ADR-0063)."
);
db_id!(
    ViewId,
    "Stable identifier of a view definition on one database (ADR-0064)."
);

/// The name a new database's title column is born with. Notion's word, and the
/// only string this build invents here: it is renameable like any other
/// property (`Change::PropertyRenamed`), so nothing keys on it.
pub const TITLE_PROPERTY_NAME: &str = "Name";

/// One database entity (ADR-0060): what a `Database` block points at through
/// `blocks.db_ref`, and the parent of the schema and of the rows. `name` is the
/// database's own name — what a link to it says — and is deliberately not the
/// title column's name (a row's title and the database's name are two things).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub id: DatabaseId,
    pub name: String,
}

impl Database {
    pub fn new(id: DatabaseId, name: impl Into<String>) -> Self {
        Database {
            id,
            name: name.into(),
        }
    }

    /// The `title` column every database is born with, at `ord = 0` (ADR-0061).
    /// A database with no title column cannot draw a row, so the two rows this
    /// and [`Self::first_view`] build are what the insert path has to write
    /// together with the `databases` row itself.
    pub fn title_property(&self, id: PropertyId) -> Property {
        Property {
            id,
            db: self.id,
            name: TITLE_PROPERTY_NAME.into(),
            kind: PropertyKind::Title,
            config: String::new(),
            ord: OrderKey::FIRST,
        }
    }

    /// The first view every database is born with (ADR-0061): a table, whose
    /// empty `definition` is "no rules" — the document that would hold filters
    /// and sorts is ADR-0064's, and absent means show everything.
    pub fn first_view(&self, id: ViewId) -> View {
        View {
            id,
            db: self.id,
            name: ViewLayout::Table.label().into(),
            layout: ViewLayout::Table,
            definition: String::new(),
            ord: OrderKey::FIRST,
        }
    }
}

/// One column of one database (SPEC §三十九 "property：列，带类型"). Stored as a
/// row rather than as JSON on the database, so that renaming a column is well
/// defined under `UNIQUE (db, name)` (ADR-0061).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub id: PropertyId,
    pub db: DatabaseId,
    pub name: String,
    pub kind: PropertyKind,
    /// The type's own settings, as one JSON document (ADR-0061) — exactly what
    /// SQL never filters on: a select's option list (options carry their own
    /// ids), a number's format, a rollup's target. D1 stores it verbatim; D2
    /// reads it through `core::database_property` (the option list, the two
    /// formats) and writes it back in the same shape, and the filter compiler
    /// that has to *ignore* ids of deleted properties is D4's.
    pub config: String,
    /// Where the column sits in the view's column list. A dense key like
    /// `pages.ord`, because `ord` being an app invariant is the price ADR-0061
    /// accepted for the row table — and a key with gaps is what makes moving a
    /// column between two others one `UPDATE`.
    pub ord: OrderKey,
}

/// What a column is (SPEC §三十九's list, plus the computed kinds). Stored as a
/// short stable string, like `blocks.kind` and `blocks.lang`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyKind {
    Title,
    Text,
    Number,
    Select,
    MultiSelect,
    Status,
    Date,
    Checkbox,
    Url,
    Email,
    Phone,
    Files,
    CreatedTime,
    LastEditedTime,
    /// Computed, never stored (ADR-0062).
    Formula,
    /// Computed, never stored (ADR-0062).
    Rollup,
    /// Computed, never stored (ADR-0062) — and, per §三十九's 排期前提, a
    /// pointer either at §四十's page mentions (Track 2) or at nothing.
    Relation,
}

impl PropertyKind {
    pub const ALL: [PropertyKind; 17] = [
        PropertyKind::Title,
        PropertyKind::Text,
        PropertyKind::Number,
        PropertyKind::Select,
        PropertyKind::MultiSelect,
        PropertyKind::Status,
        PropertyKind::Date,
        PropertyKind::Checkbox,
        PropertyKind::Url,
        PropertyKind::Email,
        PropertyKind::Phone,
        PropertyKind::Files,
        PropertyKind::CreatedTime,
        PropertyKind::LastEditedTime,
        PropertyKind::Formula,
        PropertyKind::Rollup,
        PropertyKind::Relation,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PropertyKind::Title => "title",
            PropertyKind::Text => "text",
            PropertyKind::Number => "number",
            PropertyKind::Select => "select",
            PropertyKind::MultiSelect => "multi_select",
            PropertyKind::Status => "status",
            PropertyKind::Date => "date",
            PropertyKind::Checkbox => "checkbox",
            PropertyKind::Url => "url",
            PropertyKind::Email => "email",
            PropertyKind::Phone => "phone",
            PropertyKind::Files => "files",
            PropertyKind::CreatedTime => "created_time",
            PropertyKind::LastEditedTime => "last_edited_time",
            PropertyKind::Formula => "formula",
            PropertyKind::Rollup => "rollup",
            PropertyKind::Relation => "relation",
        }
    }

    pub fn try_from_str(s: &str) -> Option<PropertyKind> {
        PropertyKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    /// What a stored string means in this build (ADR-0061's fold): a kind this
    /// build does not know — one a future build wrote — loads as `text` and the
    /// cell draws as text, rather than failing the whole library. `person` folds
    /// here too, which is where SPEC's 降级 lands: with no account model behind
    /// it, a person column is a text column (a local name list would be its own
    /// table, picker and merge rules for zero extra data).
    pub fn from_stored(s: &str) -> PropertyKind {
        match s {
            "person" => PropertyKind::Text,
            other => PropertyKind::try_from_str(other).unwrap_or(PropertyKind::Text),
        }
    }

    /// Whether this kind's value is a list, and therefore lives in
    /// `db_value_items` (ADR-0062) instead of in the three typed columns.
    pub fn is_list(self) -> bool {
        matches!(self, PropertyKind::MultiSelect | PropertyKind::Files)
    }

    /// The one kind a database has exactly one of, at `ord = 0` (ADR-0061) —
    /// and the column whose value is `pages.title` for a record that has a page
    /// (ADR-0063).
    pub fn is_title(self) -> bool {
        self == PropertyKind::Title
    }

    /// Whether this kind stores nothing at all and is computed for the window
    /// at projection time — `formula` / `rollup` / `relation` (ADR-0062). These
    /// are the kinds this build has no engine for yet, so a cell of one of them
    /// paints nothing.
    pub fn is_computed(self) -> bool {
        matches!(
            self,
            PropertyKind::Formula | PropertyKind::Rollup | PropertyKind::Relation
        )
    }

    /// Whether this kind's value is derived from something the store already
    /// keeps, and therefore is **never** written into `db_values` (ADR-0039's
    /// discipline, ADR-0068's landing): `created time` and `last edited time`
    /// are the record's own two instants, which live on `db_records` and are
    /// stamped by the write path. A cell of one of these paints the record's
    /// column and nothing else, whatever a rogue caller wrote.
    pub fn is_derived(self) -> bool {
        matches!(
            self,
            PropertyKind::CreatedTime | PropertyKind::LastEditedTime
        )
    }

    /// Which SQL column this kind's sort is an order over (ADR-0069), or `None`
    /// when the kind has no order a user would recognise. The distinction
    /// matters because the *column* decides the comparison: `number` sorts in
    /// SQLite's `REAL` column and `2` comes before `10`, while a text-stored
    /// kind sorts its bytes — which is right for a date only because ADR-0062
    /// stores one fixed-width.
    ///
    /// `None` for the list kinds: "sort by multi-select" means an order over
    /// the *options*, which is a question about the column's settings and not
    /// about the value, and D4/D5 is where that gets a meaning.
    pub fn sort_column(self) -> Option<SortColumn> {
        match self {
            PropertyKind::Title
            | PropertyKind::Text
            | PropertyKind::Url
            | PropertyKind::Email
            | PropertyKind::Phone
            | PropertyKind::Select
            | PropertyKind::Status
            | PropertyKind::Date => Some(SortColumn::Text),
            PropertyKind::Number => Some(SortColumn::Number),
            PropertyKind::Checkbox => Some(SortColumn::Flag),
            PropertyKind::CreatedTime => Some(SortColumn::Created),
            PropertyKind::LastEditedTime => Some(SortColumn::Edited),
            PropertyKind::MultiSelect
            | PropertyKind::Files
            | PropertyKind::Formula
            | PropertyKind::Rollup
            | PropertyKind::Relation => None,
        }
    }

    /// The value the store can hold for a kind, given a cell to write. The
    /// title column is text: its *storage* differs per record (page title or
    /// value row, ADR-0063) but what a caller writes into it is a string either
    /// way.
    pub fn value_kind(self) -> ValueKind {
        match self {
            PropertyKind::Number => ValueKind::Number,
            PropertyKind::Checkbox => ValueKind::Flag,
            PropertyKind::MultiSelect | PropertyKind::Files => ValueKind::Items,
            PropertyKind::Formula | PropertyKind::Rollup | PropertyKind::Relation => {
                ValueKind::Computed
            }
            PropertyKind::CreatedTime | PropertyKind::LastEditedTime => ValueKind::Derived,
            _ => ValueKind::Text,
        }
    }
}

/// Which of ADR-0062's storage shapes a kind's value takes: the `text` column,
/// the `num` column, the `flag` column, the `db_value_items` table, nothing at
/// all because the column is computed, or nothing at all because the value is
/// derived from the record itself (ADR-0068).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Number,
    Flag,
    Items,
    Computed,
    Derived,
}

/// Which column one sort orders by (ADR-0069). `Created` / `Edited` are the
/// record's own two instants, which is why a sort by them needs no join at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// The `text` column — bytes, which is time order for the two date-shaped
    /// kinds because ADR-0062 stores them fixed-width.
    Text,
    /// The `num` column: numbers, not the strings that spell them.
    Number,
    /// The `flag` column: `false` before `true`.
    Flag,
    /// `db_records.created`.
    Created,
    /// `db_records.edited`.
    Edited,
}

/// One compiled `ORDER BY` term: which stored column of which property, and
/// which way. Compiled in `core` (this is the *decision*) and turned into SQL by
/// `storage::database_store` (that is the *statement*) — §三十九's "filter and
/// sort happen in SQL, not in the UI" is only true if the order is emitted
/// there, so nothing in this crate sorts a `Vec` of rows.
///
/// One term, not a list: D2 sorts by one column, and the view document's
/// `sorts` array (ADR-0064) is D4's to compile into as many terms as it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub property: PropertyId,
    pub column: SortColumn,
    pub descending: bool,
}

impl SortSpec {
    /// The sort a column can offer, or `None` when the kind has no order a user
    /// would recognise. Asking for a sort a kind cannot give is a caller error
    /// with no sensible fallback (silently ordering by row position would look
    /// like it worked), so the answer is the absence of a `SortSpec` rather
    /// than a defaulted one.
    pub fn of(property: &Property, descending: bool) -> Option<SortSpec> {
        property.kind.sort_column().map(|column| SortSpec {
            property: property.id,
            column,
            descending,
        })
    }
}

/// A record's two instants, in ADR-0062's stored date shape — `YYYY-MM-DDTHH:MM`
/// local wall time, `""` for a record from before ADR-0068's step (which is
/// also what "unknown" paints as). Read-only on purpose: nothing hands these to
/// a `Change` (ADR-0068), so no caller can invent a birthday — the write path
/// stamps them and the derived kinds project them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordTimestamps {
    pub created: String,
    pub edited: String,
}

/// One row of one database (SPEC §三十九 "record：一行，可以同时是一个 page").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: RecordId,
    pub db: DatabaseId,
    /// The page this record is the face of (ADR-0063), or `None` for a bare
    /// record — which is how every record is born: a page arrives when someone
    /// opens the row. `UNIQUE (page)` makes the ownership mutual, so two
    /// records can never share one.
    pub page: Option<PageId>,
    /// Row order in the database's own listing, before any view's sort. A dense
    /// key rather than a rank, so inserting a row between two others is one
    /// `UPDATE` and not a renumber of every row below it.
    pub ord: OrderKey,
}

impl Record {
    /// A bare record at `ord` — the shape the "new row" path writes, and the
    /// only shape this build creates (ADR-0063's lazy page).
    pub fn bare(id: RecordId, db: DatabaseId, ord: OrderKey) -> Self {
        Record {
            id,
            db,
            page: None,
            ord,
        }
    }
}

/// SPEC §三十九's eight view layouts, in the order the app means to implement
/// them (ADR-0060 made them one entity's layouts, not eight block kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewLayout {
    Table,
    Board,
    List,
    Calendar,
    Gallery,
    Timeline,
    Form,
    Chart,
}

impl ViewLayout {
    pub const ALL: [ViewLayout; 8] = [
        ViewLayout::Table,
        ViewLayout::Board,
        ViewLayout::List,
        ViewLayout::Calendar,
        ViewLayout::Gallery,
        ViewLayout::Timeline,
        ViewLayout::Form,
        ViewLayout::Chart,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ViewLayout::Table => "table",
            ViewLayout::Board => "board",
            ViewLayout::List => "list",
            ViewLayout::Calendar => "calendar",
            ViewLayout::Gallery => "gallery",
            ViewLayout::Timeline => "timeline",
            ViewLayout::Form => "form",
            ViewLayout::Chart => "chart",
        }
    }

    pub fn try_from_str(s: &str) -> Option<ViewLayout> {
        ViewLayout::ALL.iter().copied().find(|l| l.as_str() == s)
    }

    /// What the view switcher shows. `Form` keeps its one-word name; the layout
    /// strings are the store's, these are the user's.
    pub fn label(self) -> &'static str {
        match self {
            ViewLayout::Table => "Table",
            ViewLayout::Board => "Board",
            ViewLayout::List => "List",
            ViewLayout::Calendar => "Calendar",
            ViewLayout::Gallery => "Gallery",
            ViewLayout::Timeline => "Timeline",
            ViewLayout::Form => "Form",
            ViewLayout::Chart => "Chart",
        }
    }

    /// What a stored string means in this build: an unknown layout is a table,
    /// the same fold `Lang` gives an unknown fence — a view that cannot be
    /// opened is worse than one that opens in the wrong shape.
    pub fn from_stored(s: &str) -> ViewLayout {
        ViewLayout::try_from_str(s).unwrap_or(ViewLayout::Table)
    }
}

/// One view of one database (SPEC §三十九 "view：同一份数据的一个投影").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub id: ViewId,
    pub db: DatabaseId,
    pub name: String,
    pub layout: ViewLayout,
    /// Filter + sorts + groups + visible columns + widths as one JSON document
    /// (ADR-0064): `{"v":1,"filter":…,"sorts":[…],"groups":[…],"columns":[…],
    /// "widths":{…}}`. Stored verbatim and never interpreted here; a document
    /// that does not parse degrades to "no rules" in D4's compiler, so the view
    /// opens showing everything rather than failing to open.
    pub definition: String,
    /// Where the view sits in the switcher. The order is the user's, so it is a
    /// column and not the id's order.
    pub ord: OrderKey,
}

/// One cell's value, in the shape ADR-0062 stores it: a row in `db_values`
/// using whichever typed column the property's kind names, a row per item in
/// `db_value_items` for the list kinds, or no row at all.
#[derive(Debug, Clone)]
pub enum CellValue {
    /// The absence of the value: no `db_values` row and no items. ADR-0062's
    /// one representation of "empty" — a number is never `0` for empty, and a
    /// text cell the user deliberately blanked is `Text("")`, which is a row.
    /// An empty [`CellValue::Items`] is normalised to this on the way in, so a
    /// list cell with nothing in it is the same absence as every other.
    Empty,
    /// title / text / url / email / phone / select-option-id / status-option-id
    /// / date-as-ISO-text: ADR-0062's `text` column.
    Text(String),
    /// number: the `num` column, which is why sorting by it is numeric in SQL.
    Number(f64),
    /// checkbox: the `flag` column.
    Flag(bool),
    /// multi-select option ids and files attachment ids, in display order: one
    /// `db_value_items` row each, so "has this option" is an index probe.
    Items(Vec<String>),
}

impl CellValue {
    pub fn is_empty(&self) -> bool {
        matches!(self, CellValue::Empty)
    }

    /// The painted form of a cell **with no column around it** — the value's own
    /// form, which is what a caller with no `Property` in hand can ask for. A
    /// cell on screen goes through `core::database_property::paint` instead,
    /// which is this plus the column's settings (an option's *name*, a number's
    /// format, a file's name), and which falls back to this for every kind that
    /// has no settings (ADR-0069). The two agree about a number by construction:
    /// both print it with `Display`.
    pub fn display(&self) -> String {
        match self {
            CellValue::Empty => String::new(),
            CellValue::Text(text) => text.clone(),
            // Rust's `Display` for f64 gives "3" for 3.0 and "3.5" for 3.5, so
            // a whole number does not paint a trailing `.0` (the user's number
            // *format* is the property's `config`, D2's).
            CellValue::Number(num) => format!("{num}"),
            CellValue::Flag(true) => "Yes".into(),
            CellValue::Flag(false) => "No".into(),
            CellValue::Items(items) => items.join(", "),
        }
    }
}

/// Numbers compare by bit pattern, so a value can ride inside `Change` — which
/// is `Eq` because `core::document::Entry` is, and `Entry` is what an undo step
/// is. Under bit equality `Eq` is truthful (it is an equivalence relation):
/// `NaN` equals itself and `0.0` differs from `-0.0`, which is the right answer
/// for "is this the same stored value".
impl PartialEq for CellValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CellValue::Empty, CellValue::Empty) => true,
            (CellValue::Text(a), CellValue::Text(b)) => a == b,
            (CellValue::Number(a), CellValue::Number(b)) => a.to_bits() == b.to_bits(),
            (CellValue::Flag(a), CellValue::Flag(b)) => a == b,
            (CellValue::Items(a), CellValue::Items(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for CellValue {}

/// Everything about the database layer that a startup load carries: the
/// entities, their columns, their views. **Not their records and not their
/// cells** — ADR-0067: a row exists only inside a window, so a database with
/// 10 000 rows costs a schema and a `COUNT(*)` until someone scrolls, and the
/// row objects that do appear are the ones the viewport asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DatabaseCatalog {
    pub databases: Vec<Database>,
    pub properties: Vec<Property>,
    pub views: Vec<View>,
}

impl DatabaseCatalog {
    pub fn database(&self, id: DatabaseId) -> Option<&Database> {
        self.databases.iter().find(|d| d.id == id)
    }

    /// One database's columns, in the order the store returned them (`ord`).
    pub fn properties_of(&self, db: DatabaseId) -> impl Iterator<Item = &Property> {
        self.properties.iter().filter(move |p| p.db == db)
    }

    /// The database's one `title` column (ADR-0061). `None` means the invariant
    /// was broken outside the app (SQL, or a hand-edited file) — a state the
    /// read paths survive by drawing an empty title rather than by failing.
    pub fn title_property(&self, db: DatabaseId) -> Option<&Property> {
        self.properties_of(db).find(|p| p.kind.is_title())
    }

    /// One database's views, in switcher order.
    pub fn views_of(&self, db: DatabaseId) -> impl Iterator<Item = &View> {
        self.views.iter().filter(move |v| v.db == db)
    }
}

/// What one window read needs to know: which database, which `title` column
/// (the caller read it out of the catalog a moment ago, and ADR-0063's
/// `COALESCE` cannot be written without it), the columns the view shows, in
/// the order the row's cells come back in, and the column the view is ordered
/// by. The title column may appear in `columns` or not — a row's title is
/// [`RowView::title`] either way.
#[derive(Debug, Clone, Copy)]
pub struct RowRequest<'a> {
    pub db: DatabaseId,
    pub title: PropertyId,
    pub columns: &'a [Property],
    /// The order the rows come back in, as SQL was told to produce it
    /// (ADR-0069). `None` is the database's own listing order — `db_records.ord`
    /// — which is also every sort's tie-break, so a view is never in an order
    /// nothing defined.
    pub sort: Option<SortSpec>,
}

impl<'a> RowRequest<'a> {
    /// The request a fresh view makes: the columns, no sort.
    pub fn new(db: DatabaseId, title: PropertyId, columns: &'a [Property]) -> Self {
        RowRequest {
            db,
            title,
            columns,
            sort: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid's own numbers: `benchmarks/scripts/bench.ps1` renders at
    /// 1280×800 and the editor column is a 32 px row.
    const GEOMETRY: ViewGeometry = ViewGeometry {
        row_height: 32.0,
        viewport_height: 720.0,
        overscan: DEFAULT_OVERSCAN,
    };

    fn rows_in(window: RowWindow) -> Vec<RowView> {
        (window.start..window.end)
            .map(|i| RowView {
                record: i as u64,
                title: format!("row {i}"),
                cells: vec!["a".into(), "b".into()],
            })
            .collect()
    }

    /// The number D0 owes, asserted rather than printed: 10 000 rows in, 31 row
    /// objects out, and the 31 is the viewport's arithmetic (23 visible + 8
    /// over + 8 under at the top) — not the table's size. The same table at
    /// 1 000 rows realizes the same 31.
    #[test]
    fn a_view_of_ten_thousand_rows_realizes_its_window_and_not_the_table() {
        let realized = RealizedRows::scroll_to(10_000, GEOMETRY, 0.0, rows_in);
        assert_eq!(realized.total(), 10_000);
        assert_eq!(realized.window(), RowWindow { start: 0, end: 31 });
        assert_eq!(realized.realized(), 31);
        // The window is bounded by the viewport, not by the table: a hundredth
        // of the table realizes the same slice.
        assert_eq!(window(100, GEOMETRY, 0.0), RowWindow { start: 0, end: 31 });
        assert_eq!(window(10_000, GEOMETRY, 0.0), window(1_000_000, GEOMETRY, 0.0));
    }

    /// The window is the query: `LIMIT` and `OFFSET` come out of it, and the
    /// rows that exist are the rows the fetch was told to return.
    #[test]
    fn the_window_is_the_limit_and_offset_the_fetch_runs() {
        let asked = std::cell::Cell::new(None);
        let realized = RealizedRows::scroll_to(10_000, GEOMETRY, 4_000.0, |w| {
            asked.set(Some(w));
            rows_in(w)
        });
        assert_eq!(asked.get(), Some(RowWindow { start: 117, end: 156 }));
        assert_eq!(realized.fetch(), (39, 117));
        assert_eq!(realized.realized(), 39);
        assert_eq!(realized.rows()[0].record, 117);
        assert_eq!(realized.rows()[38].record, 155);
        // A short read is not a longer model, and an empty read is not a panic.
        let short = RealizedRows::scroll_to(10_000, GEOMETRY, 0.0, |_| Vec::new());
        assert_eq!(short.fetch(), (31, 0));
        assert_eq!(short.realized(), 0);
    }

    /// Scrolled to its end the table shows its last screenful — which is a
    /// whole window, because the offset is clamped to the content the way
    /// Slint clamps it, and not a window past the end.
    #[test]
    fn the_bottom_of_the_table_is_a_screenful_and_not_the_overshoot() {
        let bottom = max_scroll_y(10_000, GEOMETRY);
        assert_eq!(bottom, 319_280.0);
        assert_eq!(
            window(10_000, GEOMETRY, bottom),
            RowWindow {
                start: 9_969,
                end: 10_000
            }
        );
        // An offset that outlives the table it was measured on still lands
        // inside it, on a whole screenful rather than on the tail.
        assert_eq!(
            window(10_000, GEOMETRY, 10_000_000.0),
            RowWindow {
                start: 9_969,
                end: 10_000
            }
        );
        assert_eq!(window(5, GEOMETRY, 10_000_000.0), RowWindow { start: 0, end: 5 });
    }

    /// A table smaller than its viewport realizes what it has and stops — the
    /// overscan never reaches outside `0..total`, and a table that cannot
    /// scroll ignores an offset instead of dropping the rows at the top.
    #[test]
    fn a_table_smaller_than_its_viewport_realizes_every_row_it_has() {
        assert_eq!(window(12, GEOMETRY, 0.0), RowWindow { start: 0, end: 12 });
        assert_eq!(window(12, GEOMETRY, 4_000.0), RowWindow { start: 0, end: 12 });
        assert_eq!(window(1, GEOMETRY, 0.0), RowWindow { start: 0, end: 1 });
        assert_eq!(max_scroll_y(12, GEOMETRY), 0.0);
    }

    /// An empty table realizes nothing, and says so instead of asking for a row
    /// that does not exist.
    #[test]
    fn an_empty_table_realizes_nothing() {
        let realized = RealizedRows::scroll_to(0, GEOMETRY, 0.0, |w| {
            assert!(w.is_empty());
            Vec::new()
        });
        assert_eq!(realized.window(), RowWindow { start: 0, end: 0 });
        assert_eq!(realized.fetch(), (0, 0));
        assert!(!RowWindow { start: 0, end: 31 }.is_empty());
        assert!(RowWindow { start: 5, end: 5 }.is_empty());
    }

    /// The two degenerate geometries have to land somewhere sane rather than
    /// divide by zero or realize nothing: an unmeasured row (Slint reports 0
    /// height on the first pass) over-realizes one frame, and a viewport
    /// shorter than one row still realizes that row.
    #[test]
    fn a_degenerate_geometry_is_bounded_and_never_panics() {
        let unmeasured = ViewGeometry::new(0.0, 720.0);
        assert_eq!(unmeasured.step(), MIN_ROW_HEIGHT);
        // 720 visible rows plus the overscan, for one frame: that is the
        // fallback's whole cost, and it is still bounded by the table.
        assert_eq!(window(10_000, unmeasured, 0.0).len(), 720 + DEFAULT_OVERSCAN);
        assert_eq!(window(100, unmeasured, 0.0).len(), 100);
        let sliver = ViewGeometry::new(40.0, 10.0);
        assert_eq!(window(10_000, sliver, 80.0), RowWindow { start: 0, end: 11 });
        assert_eq!(
            window(10_000, ViewGeometry::new(f32::NAN, 720.0), 0.0).len(),
            720 + DEFAULT_OVERSCAN
        );
    }

    // ─── D1's model ────────────────────────────────────────────────────────

    #[test]
    fn property_kind_strings_round_trip_and_unknown_ones_fold_to_text() {
        for kind in PropertyKind::ALL {
            assert_eq!(PropertyKind::try_from_str(kind.as_str()), Some(kind));
            assert_eq!(PropertyKind::from_stored(kind.as_str()), kind);
        }
        assert_eq!(PropertyKind::try_from_str("quantum"), None);
        // ADR-0061's fold: a library written by a build that knows more than
        // this one still opens, and the column draws as text.
        assert_eq!(PropertyKind::from_stored("quantum"), PropertyKind::Text);
        assert_eq!(PropertyKind::from_stored(""), PropertyKind::Text);
        // SPEC's 降级 has one home, and it is this fold.
        assert_eq!(PropertyKind::from_stored("person"), PropertyKind::Text);
        assert!(PropertyKind::ALL.iter().all(|k| k.as_str() != "person"));
    }

    #[test]
    fn each_kind_names_the_column_or_table_its_value_lives_in() {
        assert_eq!(PropertyKind::Number.value_kind(), ValueKind::Number);
        assert_eq!(PropertyKind::Checkbox.value_kind(), ValueKind::Flag);
        for kind in [PropertyKind::MultiSelect, PropertyKind::Files] {
            assert_eq!(kind.value_kind(), ValueKind::Items);
            assert!(kind.is_list());
        }
        for kind in [
            PropertyKind::Formula,
            PropertyKind::Rollup,
            PropertyKind::Relation,
        ] {
            assert_eq!(kind.value_kind(), ValueKind::Computed);
            assert!(kind.is_computed());
            assert!(!kind.is_derived(), "computed and derived are two answers");
        }
        // D2's landing of §三十九's last two kinds (ADR-0068): they store no
        // cell either, and D1's placeholder ("a derived kind is text until D2
        // says otherwise") is exactly what this assertion replaces.
        for kind in [PropertyKind::CreatedTime, PropertyKind::LastEditedTime] {
            assert_eq!(kind.value_kind(), ValueKind::Derived);
            assert!(kind.is_derived());
            assert!(!kind.is_computed(), "computed and derived are two answers");
        }
        // Everything else is a string in the `text` column — including the
        // title, whose *storage* differs per record (ADR-0063) but whose value
        // is a string either way.
        for kind in [
            PropertyKind::Title,
            PropertyKind::Text,
            PropertyKind::Select,
            PropertyKind::Status,
            PropertyKind::Date,
            PropertyKind::Url,
            PropertyKind::Email,
            PropertyKind::Phone,
        ] {
            assert_eq!(kind.value_kind(), ValueKind::Text);
        }
        assert!(PropertyKind::Title.is_title());
        assert_eq!(
            PropertyKind::ALL.iter().filter(|k| k.is_title()).count(),
            1,
            "the title kind exists once, and a database has one of it"
        );
        // A value lives in exactly one place: no kind is two of text, number,
        // flag, list, computed and derived at once.
        for kind in PropertyKind::ALL {
            assert!(!(kind.is_list() && kind.is_computed()), "{kind:?}");
            assert!(!(kind.is_list() && kind.is_derived()), "{kind:?}");
            assert!(!(kind.is_computed() && kind.is_derived()), "{kind:?}");
        }
    }

    /// Which column a sort orders by, and which kinds refuse to offer one
    /// (ADR-0069). The refusal matters as much as the answer: ordering a
    /// multi-select by "the bytes of its first option" would look like it
    /// worked, which is why the caller gets no `SortSpec` to pass on.
    #[test]
    fn a_sort_names_the_stored_column_its_kind_compares_in() {
        let of = |kind| SortSpec::of(&property(7, kind), false);
        for kind in [
            PropertyKind::Title,
            PropertyKind::Text,
            PropertyKind::Url,
            PropertyKind::Email,
            PropertyKind::Phone,
            PropertyKind::Select,
            PropertyKind::Status,
            PropertyKind::Date,
        ] {
            assert_eq!(of(kind).unwrap().column, SortColumn::Text, "{kind:?}");
        }
        assert_eq!(of(PropertyKind::Number).unwrap().column, SortColumn::Number);
        assert_eq!(of(PropertyKind::Checkbox).unwrap().column, SortColumn::Flag);
        assert_eq!(
            of(PropertyKind::CreatedTime).unwrap().column,
            SortColumn::Created
        );
        assert_eq!(
            of(PropertyKind::LastEditedTime).unwrap().column,
            SortColumn::Edited
        );
        for kind in [
            PropertyKind::MultiSelect,
            PropertyKind::Files,
            PropertyKind::Formula,
            PropertyKind::Rollup,
            PropertyKind::Relation,
        ] {
            assert_eq!(of(kind), None, "{kind:?} offered a sort");
        }
        let spec = SortSpec::of(&property(7, PropertyKind::Number), true).unwrap();
        assert_eq!(spec.property, PropertyId(7), "the column, not the kind alone");
        assert!(spec.descending, "the direction is part of the term");
    }

    fn property(id: u64, kind: PropertyKind) -> Property {
        Property {
            id: PropertyId(id),
            db: DatabaseId(1),
            name: "P".into(),
            kind,
            config: String::new(),
            ord: OrderKey::FIRST,
        }
    }

    #[test]
    fn a_view_layout_round_trips_and_an_unknown_one_is_a_table() {
        for layout in ViewLayout::ALL {
            assert_eq!(ViewLayout::try_from_str(layout.as_str()), Some(layout));
            assert_eq!(ViewLayout::from_stored(layout.as_str()), layout);
            assert!(!layout.label().is_empty());
        }
        assert_eq!(ViewLayout::try_from_str("kanban"), None);
        assert_eq!(ViewLayout::from_stored("kanban"), ViewLayout::Table);
        // SPEC's order is the implementation order, and the store's strings
        // are not the labels: they are stable spellings nothing may renumber.
        assert_eq!(ViewLayout::ALL[0], ViewLayout::Table);
        assert_eq!(ViewLayout::ALL[7], ViewLayout::Chart);
    }

    #[test]
    fn a_new_database_is_born_with_a_title_column_and_a_table_view() {
        let db = Database::new(DatabaseId(7), "Tasks");
        assert_eq!(db.id, DatabaseId(7));
        assert_eq!(db.name, "Tasks");

        let title = db.title_property(PropertyId(1));
        assert!(title.kind.is_title());
        assert_eq!(title.ord, OrderKey::FIRST, "the title column is first");
        assert_eq!(title.name, TITLE_PROPERTY_NAME);
        assert_eq!(title.db, db.id);
        assert_eq!(title.config, "", "a title column has no settings to keep");

        let view = db.first_view(ViewId(1));
        assert_eq!(view.layout, ViewLayout::Table);
        assert_eq!(view.name, "Table");
        assert_eq!(view.db, db.id);
        // No rules is the empty document, which is also what "show everything"
        // means to ADR-0064's reader.
        assert_eq!(view.definition, "");
    }

    #[test]
    fn a_cell_value_compares_by_bit_pattern_and_paints_itself() {
        assert_eq!(CellValue::Empty, CellValue::Empty);
        assert_ne!(CellValue::Empty, CellValue::Text(String::new()));
        assert_eq!(CellValue::Text("a".into()), CellValue::Text("a".into()));
        assert_ne!(CellValue::Text("a".into()), CellValue::Flag(true));
        assert_eq!(CellValue::Number(1.5), CellValue::Number(1.5));
        // Bits, not `PartialEq`'s float rules: `Entry` is `Eq`, so this has to
        // be too, and NaN equals itself under "the same stored value".
        assert_eq!(CellValue::Number(f64::NAN), CellValue::Number(f64::NAN));
        assert_ne!(CellValue::Number(0.0), CellValue::Number(-0.0));
        assert_eq!(
            CellValue::Items(vec!["a".into(), "b".into()]),
            CellValue::Items(vec!["a".into(), "b".into()])
        );

        assert_eq!(CellValue::Empty.display(), "");
        assert_eq!(CellValue::Empty.is_empty(), true);
        assert_eq!(CellValue::Text("hi".into()).display(), "hi");
        // A whole number does not paint a trailing `.0`…
        assert_eq!(CellValue::Number(3.0).display(), "3");
        assert_eq!(CellValue::Number(3.5).display(), "3.5");
        assert_eq!(CellValue::Number(-2.0).display(), "-2");
        // …and the checkbox words are ADR-0065's, so the cell and the exported
        // table say the same thing.
        assert_eq!(CellValue::Flag(true).display(), "Yes");
        assert_eq!(CellValue::Flag(false).display(), "No");
        // D1's placeholder rendering: ids until D2 reads the property's config.
        assert_eq!(
            CellValue::Items(vec!["7".into(), "9".into()]).display(),
            "7, 9"
        );
    }

    #[test]
    fn a_catalog_answers_for_one_database_at_a_time() {
        let one = Database::new(DatabaseId(1), "One");
        let two = Database::new(DatabaseId(2), "Two");
        let mut catalog = DatabaseCatalog::default();
        catalog.databases = vec![one.clone(), two.clone()];
        catalog.properties = vec![
            one.title_property(PropertyId(1)),
            two.title_property(PropertyId(2)),
            Property {
                id: PropertyId(3),
                db: two.id,
                name: "Status".into(),
                kind: PropertyKind::Status,
                config: r#"{"options":[]}"#.into(),
                ord: OrderKey(OrderKey::FIRST.0 + 2),
            },
        ];
        catalog.views = vec![one.first_view(ViewId(1)), two.first_view(ViewId(2))];

        assert_eq!(catalog.database(DatabaseId(2)).map(|d| d.name.as_str()), Some("Two"));
        assert_eq!(catalog.database(DatabaseId(9)), None);
        assert_eq!(
            catalog.properties_of(DatabaseId(1)).map(|p| p.id).collect::<Vec<_>>(),
            vec![PropertyId(1)]
        );
        assert_eq!(
            catalog.properties_of(DatabaseId(2)).map(|p| p.id).collect::<Vec<_>>(),
            vec![PropertyId(2), PropertyId(3)]
        );
        assert_eq!(
            catalog.title_property(DatabaseId(2)).map(|p| p.id),
            Some(PropertyId(2))
        );
        assert_eq!(
            catalog.views_of(DatabaseId(1)).map(|v| v.layout).collect::<Vec<_>>(),
            vec![ViewLayout::Table]
        );
        // The list-valued column keeps its config verbatim: D1 does not read
        // inside it (D2 does), and it never rewrites what it stores.
        assert_eq!(
            catalog.properties_of(DatabaseId(2)).nth(1).map(|p| p.config.as_str()),
            Some(r#"{"options":[]}"#)
        );
        // A database whose title column is missing is survivable at read time:
        // the answer is `None`, and the caller draws an empty title.
        assert_eq!(catalog.title_property(DatabaseId(9)), None);
    }
}

/// D0's measurement. Two windows on the same table — one that holds every row
/// and one that holds the window — and the bytes each costs, counted on the
/// measuring thread by a transparent global allocator.
///
/// Reachable from D1's probe as well (`storage::database_store`), which weighs
/// real SQL rows instead of built ones: there is exactly one global allocator
/// per test binary, so the counter and the arming live here and are shared.
#[cfg(test)]
pub(crate) mod probe {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::time::Instant;

    thread_local! {
        // `const`-initialized so that reading them from inside `alloc` cannot
        // itself allocate: a lazy `thread_local!` would recurse.
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static LIVE: Cell<isize> = const { Cell::new(0) };
    }

    /// The system allocator, plus a live-byte counter for whichever thread is
    /// measuring. Other test threads run in parallel and are not armed, so the
    /// count is this thread's alone — which is the only way to weigh one
    /// structure inside a test binary that shares an allocator with the harness.
    struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc(layout);
            if !ptr.is_null() {
                count(layout.size() as isize);
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc_zeroed(layout);
            if !ptr.is_null() {
                count(layout.size() as isize);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            count(-(layout.size() as isize));
            System.dealloc(ptr, layout);
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let out = System.realloc(ptr, layout, new_size);
            if !out.is_null() {
                count(new_size as isize - layout.size() as isize);
            }
            out
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    fn count(delta: isize) {
        let _ = ARMED.try_with(|armed| {
            if armed.get() {
                let _ = LIVE.try_with(|live| live.set(live.get() + delta));
            }
        });
    }

    /// Run `f` and report what its result holds on the heap, in bytes. Shared
    /// with D1's storage probe; the counter is this thread's alone.
    pub(crate) fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        LIVE.with(|live| live.set(0));
        ARMED.with(|armed| armed.set(true));
        let out = f();
        let bytes = LIVE.with(|live| live.get()).max(0) as usize;
        ARMED.with(|armed| armed.set(false));
        (out, bytes)
    }

    fn row(i: usize) -> RowView {
        RowView {
            record: i as u64,
            title: format!("Note {i}"),
            cells: (0..5).map(|c| format!("v{c}-{i}")).collect(),
        }
    }

    /// What a 10 000-row table costs the process it is opened in, four ways.
    /// Printed, not asserted beyond its shape: nothing else in the repo can
    /// weigh a database yet (the app has no database view until D3), so this is
    /// the number D0 owes and the one D8 will compare against.
    #[test]
    #[ignore = "prints a measurement; run with --release --ignored --nocapture"]
    fn a_window_costs_its_rows_and_a_table_costs_all_of_them() {
        const TOTAL: usize = 10_000;
        let geometry = ViewGeometry::new(32.0, 720.0);
        let before = counters::process_bytes();

        let started = Instant::now();
        let (all, all_bytes) = measure(|| (0..TOTAL).map(row).collect::<Vec<_>>());
        let all_ms = started.elapsed().as_secs_f64() * 1e3;

        let started = Instant::now();
        let (ids, ids_bytes) = measure(|| (0..TOTAL as u64).collect::<Vec<u64>>());
        let ids_ms = started.elapsed().as_secs_f64() * 1e3;

        let started = Instant::now();
        let (realized, window_bytes) =
            measure(|| RealizedRows::scroll_to(TOTAL, geometry, 0.0, |w| rows_of(w)));
        let window_ms = started.elapsed().as_secs_f64() * 1e3;
        let after = counters::process_bytes();

        let rows = realized.realized();
        assert_eq!(rows, 31, "the window is the viewport's arithmetic");
        assert!(all.len() == TOTAL && ids.len() == TOTAL);
        // The claim in one line: the window's rows cost a fraction of the
        // table's, and the margin is the table's size over the window's.
        assert!(
            window_bytes * 10 < all_bytes,
            "window {window_bytes} B vs table {all_bytes} B"
        );

        let rows_mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        println!(
            "database window probe: {TOTAL} rows, geometry 32 px row / 720 px viewport \
             / {DEFAULT_OVERSCAN} overscan"
        );
        println!(
            "  realized {rows} rows (start..end = {}..{}) — the other {} rows have no object",
            realized.window().start,
            realized.window().end,
            TOTAL - rows
        );
        println!(
            "  heap: window {window_bytes} B, the table's rows {all_bytes} B \
             ({:.1}x), the table's ids only {ids_bytes} B",
            all_bytes as f64 / window_bytes.max(1) as f64
        );
        println!(
            "  build: all rows {all_ms:.3} ms, ids only {ids_ms:.3} ms, the window's fetch \
             {window_ms:.4} ms"
        );
        match (before, after) {
            (Some((ws0, priv0)), Some((ws1, priv1))) => println!(
                "  process: working set {:.1} -> {:.1} MB, private {:.1} -> {:.1} MB \
                 (Δ {:.1} / {:.1} MB)",
                rows_mb(ws0),
                rows_mb(ws1),
                rows_mb(priv0),
                rows_mb(priv1),
                rows_mb(ws1.saturating_sub(ws0)),
                rows_mb(priv1.saturating_sub(priv0)),
            ),
            _ => println!("  process: not readable on this platform"),
        }
        let (ws_mb, priv_mb) = match after {
            Some((ws, priv_bytes)) => (rows_mb(ws), rows_mb(priv_bytes)),
            None => (0.0, 0.0),
        };
        println!(
            "{{\"label\":\"track3-d0-window\",\"date\":\"2026-09-22\",\
             \"harness\":\"cargo test --release --lib -- --ignored --nocapture\",\
             \"total\":{TOTAL},\"row_height\":32.0,\"viewport_height\":720.0,\
             \"overscan\":{DEFAULT_OVERSCAN},\"realized_top\":{rows},\
             \"realized_middle\":{},\"realized_bottom\":{},\"fetch_limit\":{},\
             \"fetch_offset\":{},\"heap_window_bytes\":{window_bytes},\
             \"heap_all_rows_bytes\":{all_bytes},\"heap_ids_only_bytes\":{ids_bytes},\
             \"heap_ratio\":{:.2},\"process_working_set_mb\":{ws_mb:.1},\
             \"process_private_mb\":{priv_mb:.1}}}",
            window(TOTAL, geometry, 4_000.0).len(),
            window(TOTAL, geometry, max_scroll_y(TOTAL, geometry)).len(),
            realized.fetch().0,
            realized.fetch().1,
            all_bytes as f64 / window_bytes.max(1) as f64,
        );
    }

    fn rows_of(w: RowWindow) -> Vec<RowView> {
        (w.start..w.end).map(row).collect()
    }

    /// This process's working set and private bytes, read with the same two
    /// numbers `benchmarks/scripts/bench.ps1` reports for the app window. The
    /// two declarations are hand-written for the reason ADR-0025 gives (one
    /// `extern` block instead of a crate), and they live here rather than in
    /// `platform` because only a headless test asks for this.
    pub(crate) mod counters {
        #[repr(C)]
        #[derive(Default)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set: usize,
            working_set: usize,
            quota_peak_paged: usize,
            quota_paged: usize,
            quota_peak_non_paged: usize,
            quota_non_paged: usize,
            pagefile: usize,
            peak_pagefile: usize,
        }

        #[cfg(windows)]
        mod win {
            use super::ProcessMemoryCounters;

            #[link(name = "kernel32")]
            extern "system" {
                fn GetCurrentProcess() -> isize;
                fn K32GetProcessMemoryInfo(
                    process: isize,
                    counters: *mut ProcessMemoryCounters,
                    bytes: u32,
                ) -> i32;
            }

            /// `(working set, private bytes)` in bytes.
            pub fn process_bytes() -> Option<(usize, usize)> {
                let mut counters = ProcessMemoryCounters::default();
                counters.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
                let ok = unsafe {
                    K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb)
                };
                (ok != 0).then_some((counters.working_set, counters.pagefile))
            }
        }

        #[cfg(not(windows))]
        pub fn process_bytes() -> Option<(usize, usize)> {
            None
        }

        #[cfg(windows)]
        pub use win::process_bytes;
    }
}
