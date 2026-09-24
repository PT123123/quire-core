// The wire shape of a sync snapshot (serde structs owned by the shell).
//
// The core's own types (`core::Page`, `core::Block`, …) carry no serde
// derives, and quire-core is a pinned git dependency the shell cannot amend,
// so this module mirrors the rows it needs with plain ids (`u64`) and
// self-describing enum strings (`as_str` / `try_from_str` — the same spellings
// the SQLite columns hold). The conversion is the only place the two shapes
// meet: the merge (`super::merge`) works on these structs alone, and the
// apply (`app::state::sync_import`) reads them straight back into `Change`s.
//
// Versioning: `version` is the snapshot protocol version. A peer that speaks
// a different one is refused at the HTTP layer rather than half-understood.

use serde::{Deserialize, Serialize};

use crate::core::organizer as org;
use crate::core::types::{
    Attachment, Block, BlockId, ColorKind, Lang, Mark, MarkKind, OrderKey, Page, PageFont, PageId,
};
use crate::core::{database as db, database::DatabaseCatalog};

/// The snapshot protocol this build speaks.
///
/// **1 → 2** when SPEC §四十一's organizer landed, and the reason is not
/// housekeeping: the version gate is an exact equality, and a v2 peer's
/// `notes` / `tasks` / `lists` sent to a v1 peer would be *silently dropped* by
/// serde — the old side would answer a merged snapshot with none of them in it,
/// and the new side reads a missing id in a merged snapshot as **a delete**
/// (that is how the two-way protocol lands removals). A user's brand-new notes
/// would therefore be deleted by the first sync with an un-updated device.
///
/// So the bump makes the two builds refuse each other at `from_json` instead:
/// loud, immediate, and fixable by updating the other end. The cost is real and
/// is the reason this is an ADR rather than a footnote — both ends must be
/// updated together, and a v1 peer can no longer sync at all.
pub const SNAPSHOT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncSnapshot {
    pub version: u32,
    /// The sending device's id — how the receiver names the sharer, and the
    /// key its shadow is stored under.
    #[serde(default)]
    pub device_id: String,
    /// The sending device's readable name, for logs and conflict notes.
    #[serde(default)]
    pub device: String,
    pub pages: Vec<SPage>,
    pub blocks: Vec<SBlock>,
    pub attachments: Vec<SAttachment>,
    pub databases: Vec<SDatabase>,
    /// SPEC §四十一's three collections. **No `#[serde(default)]` on them**, on
    /// purpose: a payload that does not carry them is not a payload with an
    /// empty organizer, it is a payload this build cannot honestly merge — and
    /// the deletion detection above would turn "absent" into "deleted". The
    /// version gate is what keeps that from arriving; this is the second lock
    /// on the same door.
    pub notes: Vec<SNote>,
    pub tasks: Vec<STask>,
    pub lists: Vec<STaskList>,
}

impl Default for SyncSnapshot {
    fn default() -> Self {
        SyncSnapshot {
            version: SNAPSHOT_VERSION,
            device_id: String::new(),
            device: String::new(),
            pages: Vec::new(),
            blocks: Vec::new(),
            attachments: Vec::new(),
            databases: Vec::new(),
            notes: Vec::new(),
            tasks: Vec::new(),
            lists: Vec::new(),
        }
    }
}

impl SyncSnapshot {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| String::new())
    }

    pub fn from_json(body: &str) -> Result<SyncSnapshot, String> {
        let snap: SyncSnapshot =
            serde_json::from_str(body).map_err(|e| format!("bad snapshot: {e}"))?;
        if snap.version != SNAPSHOT_VERSION {
            return Err(format!(
                "snapshot version {} unsupported (this build speaks {})",
                snap.version, SNAPSHOT_VERSION
            ));
        }
        Ok(snap)
    }

    /// The number of carried rows, for logs.
    pub fn row_count(&self) -> usize {
        self.pages.len()
            + self.blocks.len()
            + self.attachments.len()
            + self.databases.len()
            + self.notes.len()
            + self.tasks.len()
            + self.lists.len()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SPage {
    pub id: u64,
    pub title: String,
    pub parent: Option<u64>,
    pub ord: u64,
    pub favorite: bool,
    pub expanded: bool,
    pub font: String,
    pub full_width: bool,
    pub small_text: bool,
    pub icon: String,
    pub cover: Option<u64>,
    pub locked: bool,
    pub template: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SMark {
    pub start: usize,
    pub end: usize,
    pub kind: String,
    /// The one payload column: a link/mention's url, or a date's ISO text
    /// (`Mark::stored_payload` / `Mark::from_stored` are the round trip).
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SBlock {
    pub id: u64,
    pub page: u64,
    pub parent: Option<u64>,
    pub ord: u64,
    pub kind: String,
    pub text: String,
    pub checked: bool,
    pub folded: bool,
    pub color: String,
    pub background: String,
    pub page_ref: Option<u64>,
    pub sync_ref: Option<u64>,
    pub attachment: Option<u64>,
    pub img_percent: u16,
    pub columns: u16,
    pub lang: String,
    pub db_ref: Option<u64>,
    pub marks: Vec<SMark>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SAttachment {
    pub id: u64,
    pub name: String,
    pub file: String,
    pub thumb: String,
    pub mime: String,
    pub bytes: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SProperty {
    pub id: u64,
    pub db: u64,
    pub name: String,
    pub kind: String,
    pub config: String,
    pub ord: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SView {
    pub id: u64,
    pub db: u64,
    pub name: String,
    pub layout: String,
    pub definition: String,
    pub ord: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SRecord {
    pub id: u64,
    pub db: u64,
    pub page: Option<u64>,
    pub ord: u64,
}

/// One cell as stored: the three flat columns plus the list kinds' items.
/// `None` everywhere is `CellValue::Empty` — the one representation of empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SValue {
    pub record: u64,
    pub property: u64,
    pub text: Option<String>,
    pub num: Option<f64>,
    pub flag: Option<bool>,
    pub items: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SDatabase {
    pub id: u64,
    pub name: String,
    pub template: String,
    pub properties: Vec<SProperty>,
    pub views: Vec<SView>,
    pub records: Vec<SRecord>,
    pub values: Vec<SValue>,
}

/// SPEC §四十一's three rows on the wire. Flat, like every other row here: the
/// organizer has no nesting (a task's subtasks are part of the task the way its
/// tags are, not rows of a collection the merge compares one by one), so its
/// three collections sit beside `pages` and `blocks` rather than inside a
/// wrapper. The enums travel as the strings the columns hold — the same
/// `as_str` / `try_from_str` pair `kind` and `color` already use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SNote {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub pinned: bool,
    pub tags: Vec<String>,
    /// Unix seconds. They travel because they are part of the row the merge
    /// compares: a receiving device that dropped them would write a row that
    /// differs from the sender's in two fields nobody edited, and every later
    /// sync would read that as a change.
    pub created: i64,
    pub edited: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct STaskList {
    pub id: u64,
    pub name: String,
    pub color: String,
    pub ord: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SSubtask {
    pub id: u64,
    pub title: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct STask {
    pub id: u64,
    /// The list it belongs to, `0` being the inbox sentinel (`ListId::INBOX`) —
    /// an id and not a row, exactly as the column stores it, so a task in the
    /// inbox needs no list row to arrive with it.
    pub list: u64,
    pub title: String,
    pub notes: String,
    pub priority: String,
    /// `YYYY-MM-DD`, or absent for no deadline.
    pub due: Option<String>,
    pub repeat: String,
    pub done: bool,
    pub completed_at: Option<i64>,
    pub tags: Vec<String>,
    pub subtasks: Vec<SSubtask>,
    pub created: i64,
    pub edited: i64,
    pub ord: u64,
}

// ─── conversions: core rows ↔ wire rows ─────────────────────────────────────

impl From<&Page> for SPage {
    fn from(p: &Page) -> Self {
        SPage {
            id: p.id.0,
            title: p.title.clone(),
            parent: p.parent.map(|v| v.0),
            ord: p.order.0,
            favorite: p.favorite,
            expanded: p.expanded,
            font: p.font.as_str().to_string(),
            full_width: p.full_width,
            small_text: p.small_text,
            icon: p.icon.clone(),
            cover: p.cover.map(|v| v.0),
            locked: p.locked,
            template: p.template,
        }
    }
}

impl SPage {
    pub fn to_core(&self) -> Page {
        Page {
            id: PageId(self.id),
            title: self.title.clone(),
            parent: self.parent.map(PageId),
            order: OrderKey(self.ord),
            favorite: self.favorite,
            expanded: self.expanded,
            font: PageFont::try_from_str(&self.font).unwrap_or_default(),
            full_width: self.full_width,
            small_text: self.small_text,
            icon: self.icon.clone(),
            cover: self.cover.map(crate::core::types::AttachmentId),
            locked: self.locked,
            template: self.template,
        }
    }
}

impl From<&Block> for SBlock {
    fn from(b: &Block) -> Self {
        SBlock {
            id: b.id.0,
            page: b.page.0,
            parent: b.parent.map(|v| v.0),
            ord: b.order.0,
            kind: b.kind.as_str().to_string(),
            text: b.text.clone(),
            checked: b.checked,
            folded: b.folded,
            color: b.color.as_str().to_string(),
            background: b.background.as_str().to_string(),
            page_ref: b.page_ref.map(|v| v.0),
            sync_ref: b.sync_ref.map(|v| v.0),
            attachment: b.attachment.map(|v| v.0),
            img_percent: b.img_percent,
            columns: b.columns,
            lang: b.lang.as_str().to_string(),
            db_ref: b.db_ref.map(|v| v.0),
            marks: b.marks.iter().map(Into::into).collect(),
        }
    }
}

impl SBlock {
    pub fn to_core(&self) -> Block {
        Block {
            id: BlockId(self.id),
            page: PageId(self.page),
            parent: self.parent.map(BlockId),
            order: OrderKey(self.ord),
            kind: crate::core::types::BlockKind::try_from_str(&self.kind)
                .unwrap_or(crate::core::types::BlockKind::Paragraph),
            text: self.text.clone(),
            checked: self.checked,
            marks: self.marks.iter().map(|m| m.to_core()).collect(),
            color: ColorKind::try_from_str(&self.color).unwrap_or(ColorKind::Default),
            background: ColorKind::try_from_str(&self.background).unwrap_or(ColorKind::Default),
            page_ref: self.page_ref.map(PageId),
            folded: self.folded,
            attachment: self.attachment.map(crate::core::types::AttachmentId),
            img_percent: self.img_percent,
            columns: self.columns,
            lang: Lang::try_from_str(&self.lang).unwrap_or(Lang::Plain),
            db_ref: self.db_ref.map(db::DatabaseId),
            sync_ref: self.sync_ref.map(BlockId),
        }
    }
}

impl From<&Mark> for SMark {
    fn from(m: &Mark) -> Self {
        SMark {
            start: m.start,
            end: m.end,
            kind: m.kind.as_str().to_string(),
            payload: m.stored_payload().to_string(),
        }
    }
}

impl SMark {
    pub fn to_core(&self) -> Mark {
        let kind = MarkKind::try_from_str(&self.kind).unwrap_or(MarkKind::Bold);
        Mark::from_stored(self.start, self.end, kind, self.payload.clone())
    }
}

impl From<&Attachment> for SAttachment {
    fn from(a: &Attachment) -> Self {
        SAttachment {
            id: a.id.0,
            name: a.name.clone(),
            file: a.file.clone(),
            thumb: a.thumb.clone(),
            mime: a.mime.clone(),
            bytes: a.bytes,
            width: a.width,
            height: a.height,
        }
    }
}

impl SAttachment {
    pub fn to_core(&self) -> Attachment {
        Attachment {
            id: crate::core::types::AttachmentId(self.id),
            name: self.name.clone(),
            file: self.file.clone(),
            thumb: self.thumb.clone(),
            mime: self.mime.clone(),
            bytes: self.bytes,
            width: self.width,
            height: self.height,
        }
    }
}

impl SProperty {
    pub fn to_core(&self) -> db::Property {
        db::Property {
            id: db::PropertyId(self.id),
            db: db::DatabaseId(self.db),
            name: self.name.clone(),
            kind: db::PropertyKind::try_from_str(&self.kind).unwrap_or(db::PropertyKind::Text),
            config: self.config.clone(),
            ord: OrderKey(self.ord),
        }
    }
}

impl SView {
    pub fn to_core(&self) -> db::View {
        db::View {
            id: db::ViewId(self.id),
            db: db::DatabaseId(self.db),
            name: self.name.clone(),
            layout: db::ViewLayout::try_from_str(&self.layout).unwrap_or(db::ViewLayout::Table),
            definition: self.definition.clone(),
            ord: OrderKey(self.ord),
        }
    }
}

impl SRecord {
    pub fn to_core_record(&self) -> db::Record {
        db::Record {
            id: db::RecordId(self.id),
            db: db::DatabaseId(self.db),
            page: self.page.map(crate::core::types::PageId),
            ord: OrderKey(self.ord),
        }
    }
}

impl SValue {
    pub fn to_core(&self) -> db::CellValue {
        match (&self.items, self.text.clone(), self.num, self.flag) {
            (Some(items), _, _, _) => db::CellValue::Items(items.clone()),
            (None, Some(t), _, _) => db::CellValue::Text(t),
            (None, _, Some(n), _) => db::CellValue::Number(n),
            (None, _, _, Some(f)) => db::CellValue::Flag(f),
            (None, None, None, None) => db::CellValue::Empty,
        }
    }

    pub fn from_core(record: db::RecordId, property: db::PropertyId, v: &db::CellValue) -> Self {
        let (text, num, flag, items) = match v {
            db::CellValue::Empty => (None, None, None, None),
            db::CellValue::Text(t) => (Some(t.clone()), None, None, None),
            db::CellValue::Number(n) => (None, Some(*n), None, None),
            db::CellValue::Flag(f) => (None, None, Some(*f), None),
            db::CellValue::Items(items) => (None, None, None, Some(items.clone())),
        };
        SValue {
            record: record.0,
            property: property.0,
            text,
            num,
            flag,
            items,
        }
    }
}

/// Split one catalog entity into its wire row (schema only; records and
/// values travel separately, read from the store).
pub fn database_row(d: &db::Database) -> SDatabase {
    SDatabase {
        id: d.id.0,
        name: d.name.clone(),
        template: d.template.clone(),
        properties: Vec::new(),
        views: Vec::new(),
        records: Vec::new(),
        values: Vec::new(),
    }
}

impl SDatabase {
    pub fn to_core_entity(&self) -> db::Database {
        db::Database {
            id: db::DatabaseId(self.id),
            name: self.name.clone(),
            template: self.template.clone(),
        }
    }
}

impl From<&org::Note> for SNote {
    fn from(n: &org::Note) -> Self {
        SNote {
            id: n.id.0,
            title: n.title.clone(),
            body: n.body.clone(),
            pinned: n.pinned,
            tags: n.tags.clone(),
            created: n.created,
            edited: n.edited,
        }
    }
}

impl SNote {
    pub fn to_core(&self) -> org::Note {
        org::Note {
            id: org::NoteId(self.id),
            title: self.title.clone(),
            body: self.body.clone(),
            pinned: self.pinned,
            tags: self.tags.clone(),
            created: self.created,
            edited: self.edited,
        }
    }
}

impl From<&org::TaskList> for STaskList {
    fn from(l: &org::TaskList) -> Self {
        STaskList {
            id: l.id.0,
            name: l.name.clone(),
            color: l.color.as_str().to_string(),
            ord: l.ord.0,
        }
    }
}

impl STaskList {
    pub fn to_core(&self) -> org::TaskList {
        org::TaskList {
            id: org::ListId(self.id),
            name: self.name.clone(),
            color: ColorKind::try_from_str(&self.color).unwrap_or(ColorKind::Default),
            ord: OrderKey(self.ord),
        }
    }
}

impl From<&org::Subtask> for SSubtask {
    fn from(s: &org::Subtask) -> Self {
        SSubtask {
            id: s.id,
            title: s.title.clone(),
            done: s.done,
        }
    }
}

impl SSubtask {
    pub fn to_core(&self) -> org::Subtask {
        org::Subtask {
            id: self.id,
            title: self.title.clone(),
            done: self.done,
        }
    }
}

impl From<&org::Task> for STask {
    fn from(t: &org::Task) -> Self {
        STask {
            id: t.id.0,
            list: t.list.0,
            title: t.title.clone(),
            notes: t.notes.clone(),
            priority: t.priority.as_str().to_string(),
            due: t.due.clone(),
            repeat: t.repeat.as_str().to_string(),
            done: t.done,
            completed_at: t.completed_at,
            tags: t.tags.clone(),
            subtasks: t.subtasks.iter().map(Into::into).collect(),
            created: t.created,
            edited: t.edited,
            ord: t.ord.0,
        }
    }
}

impl STask {
    pub fn to_core(&self) -> org::Task {
        org::Task {
            id: org::TaskId(self.id),
            // `0` is the inbox and stays `0`: the sentinel needs no lookup, and
            // a task that arrives in the inbox must not be sent to whichever
            // list happens to hold that id on this device.
            list: org::ListId(self.list),
            title: self.title.clone(),
            notes: self.notes.clone(),
            priority: org::Priority::from_stored(&self.priority),
            due: self.due.clone().filter(|d| !d.is_empty()),
            repeat: org::Repeat::from_stored(&self.repeat),
            done: self.done,
            completed_at: self.completed_at,
            tags: self.tags.clone(),
            subtasks: self.subtasks.iter().map(|s| s.to_core()).collect(),
            created: self.created,
            edited: self.edited,
            ord: OrderKey(self.ord),
        }
    }
}

/// The catalog's schema half (entities, columns, views) as wire rows.
pub fn catalog_schema(catalog: &DatabaseCatalog) -> Vec<SDatabase> {
    catalog
        .databases
        .iter()
        .map(database_row)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|mut row| {
            if let Some(d) = catalog.database(db::DatabaseId(row.id)) {
                row.properties = catalog
                    .properties_of(d.id)
                    .map(|p| SProperty {
                        id: p.id.0,
                        db: p.db.0,
                        name: p.name.clone(),
                        kind: p.kind.as_str().to_string(),
                        config: p.config.clone(),
                        ord: p.ord.0,
                    })
                    .collect();
                row.views = catalog
                    .views_of(d.id)
                    .map(|v| SView {
                        id: v.id.0,
                        db: v.db.0,
                        name: v.name.clone(),
                        layout: v.layout.as_str().to_string(),
                        definition: v.definition.clone(),
                        ord: v.ord.0,
                    })
                    .collect();
            }
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_round_trips_through_the_wire_row() {
        let page = Page {
            id: PageId(12),
            title: "Synced".into(),
            parent: Some(PageId(4)),
            order: OrderKey(1 << 20),
            favorite: true,
            expanded: false,
            font: PageFont::Serif,
            full_width: true,
            small_text: false,
            icon: "📄".into(),
            cover: Some(crate::core::types::AttachmentId(3)),
            locked: true,
            template: false,
        };
        let wire = SPage::from(&page);
        let back = wire.to_core();
        assert_eq!(back, page);
    }

    #[test]
    fn block_and_marks_round_trip() {
        let block = Block {
            id: BlockId(9),
            page: PageId(1),
            parent: Some(BlockId(8)),
            order: OrderKey(7),
            kind: crate::core::types::BlockKind::Todo,
            text: "buy milk".into(),
            checked: true,
            marks: vec![
                Mark {
                    start: 0,
                    end: 3,
                    kind: MarkKind::Bold,
                    url: String::new(),
                    date: None,
                },
                Mark {
                    start: 4,
                    end: 8,
                    kind: MarkKind::Date,
                    url: String::new(),
                    date: Some("2026-09-23".into()),
                },
            ],
            color: ColorKind::Gray,
            background: ColorKind::Default,
            page_ref: None,
            folded: false,
            attachment: None,
            img_percent: 100,
            columns: 0,
            lang: Lang::Plain,
            db_ref: None,
            sync_ref: None,
        };
        let back = SBlock::from(&block).to_core();
        assert_eq!(back, block);
    }

    #[test]
    fn snapshot_json_round_trips_and_checks_the_version() {
        let mut snap = SyncSnapshot::default();
        snap.device = "desk".into();
        snap.pages.push(SPage::from(&Page {
            id: PageId(2),
            title: "one".into(),
            parent: None,
            order: OrderKey(1),
            favorite: false,
            expanded: false,
            font: PageFont::default(),
            full_width: false,
            small_text: false,
            icon: String::new(),
            cover: None,
            locked: false,
            template: false,
        }));
        let json = snap.to_json();
        let back = SyncSnapshot::from_json(&json).unwrap();
        assert_eq!(back, snap);

        let mut bad = snap.clone();
        bad.version = 99;
        assert!(SyncSnapshot::from_json(&bad.to_json()).is_err());
        assert!(SyncSnapshot::from_json("not json").is_err());
        // The version right below this one is refused as well, which is the
        // whole point of the bump: a v1 peer must not be half-understood.
        let mut old = snap.clone();
        old.version = 1;
        let refusal = SyncSnapshot::from_json(&old.to_json()).unwrap_err();
        assert!(refusal.contains("version 1"), "{refusal}");
        assert!(refusal.contains("speaks 2"), "{refusal}");
    }

    /// The three organizer rows through the wire: every field, including the
    /// two instants and the checklist, because a field that does not survive
    /// this trip is a field the user loses on the other device (and, worse, one
    /// the merge then reads as an edit nobody made).
    #[test]
    fn a_note_a_list_and_a_task_round_trip_through_their_wire_rows() {
        let note = org::Note {
            id: org::NoteId(3),
            title: "Ideas".into(),
            body: "one\ntwo 中文".into(),
            pinned: true,
            tags: vec!["idea".into(), "重要".into()],
            created: 1_700_000_000,
            edited: 1_700_000_900,
        };
        assert_eq!(SNote::from(&note).to_core(), note);

        let list = org::TaskList {
            id: org::ListId(5),
            name: "Work".into(),
            color: ColorKind::Blue,
            ord: OrderKey(1 << 20),
        };
        assert_eq!(STaskList::from(&list).to_core(), list);

        let task = org::Task {
            id: org::TaskId(9),
            list: org::ListId::INBOX,
            title: "Ship it".into(),
            notes: "under the title".into(),
            priority: org::Priority::High,
            due: Some("2026-09-24".into()),
            repeat: org::Repeat::Weekdays,
            done: true,
            completed_at: Some(1_700_000_500),
            tags: vec!["work".into()],
            subtasks: vec![
                org::Subtask {
                    id: 90,
                    title: "first".into(),
                    done: true,
                },
                org::Subtask {
                    id: 91,
                    title: "second".into(),
                    done: false,
                },
            ],
            created: 1_700_000_000,
            edited: 1_700_000_900,
            ord: OrderKey(1 << 16),
        };
        assert_eq!(STask::from(&task).to_core(), task);
        // The sentinel survives as itself rather than as "whichever list holds
        // id 0 on the other device" — there is no such list anywhere.
        assert_eq!(STask::from(&task).list, 0);
        // An absent deadline is `None` on both sides of the trip, and a blank
        // string folds to the same absence rather than becoming a second
        // spelling of it.
        let mut open = task.clone();
        open.due = None;
        assert_eq!(STask::from(&open).to_core().due, None);
        let mut blank = STask::from(&open);
        blank.due = Some(String::new());
        assert_eq!(blank.to_core().due, None);
    }

    /// A payload that lost the organizer on the way is **not** a payload with an
    /// empty organizer: it fails to parse instead. This is the second lock on
    /// the door the version bump closes — the first is the version gate itself.
    #[test]
    fn a_payload_without_the_organizer_is_refused_rather_than_read_as_empty() {
        let json = SyncSnapshot::default().to_json();
        assert!(SyncSnapshot::from_json(&json).is_ok());
        for field in ["notes", "tasks", "lists"] {
            let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
            value
                .as_object_mut()
                .expect("a snapshot is an object")
                .remove(field);
            let missing = value.to_string();
            assert!(
                SyncSnapshot::from_json(&missing).is_err(),
                "a payload without {field} parsed as one with an empty organizer"
            );
        }
    }
}
