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

use crate::core::types::{
    Attachment, Block, BlockId, ColorKind, Lang, Mark, MarkKind, OrderKey, Page, PageFont, PageId,
};
use crate::core::{database as db, database::DatabaseCatalog};

/// The snapshot protocol this build speaks.
pub const SNAPSHOT_VERSION: u32 = 1;

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
        self.pages.len() + self.blocks.len() + self.attachments.len() + self.databases.len()
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
    }
}
