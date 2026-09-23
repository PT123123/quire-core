// The three-way merge (aw-server-plus's "uuid 逻辑键 + rev 仲裁" adapted to
// Quire's integer ids and snapshot exchange).
//
// Inputs: the local snapshot, the shadow (what the two peers last agreed on —
// `app::state` persists one per peer), and the remote snapshot. Output: the
// merged snapshot plus what the user should be told.
//
// Per row of every collection, over the union of the three key sets:
//
//   local == remote                      → converged; keep either
//   shadow == local (remote moved)       → take remote (an edit or a delete)
//   shadow == remote (local moved)       → keep local
//   shadow misses the key, both present,
//   and they differ                      → two devices minted the same id for
//                                          different rows since the last sync
//                                          (ids are per-device `max + 1`):
//                                          renumber the remote row and keep
//                                          both — losing a page silently is
//                                          the one outcome worse than a
//                                          duplicate
//   anything else                        → both sides edited the row: keep
//                                          local, log the conflict (the peer
//                                          logs the mirror image)
//
// Renumbering cascades: a renumbered page renumbers its blocks; a renumbered
// database renumbers its columns, views, records and cells; renumbered blocks
// pull their attachment rows along. Every remote-origin row that survives
// (taken or renumbered) then has its cross-references walked through the
// renumbering maps, so no pointer ever dangles on arrival.
//
// A first sync between two devices that both have content runs with no
// shadow: every differing row is a conflict and stays local. That is the
// honest answer — two workspaces merged for the first time are one user
// decision, not one algorithm — and the rows that only one side has still
// flow.

use super::model::{SAttachment, SBlock, SDatabase, SPage, SProperty, SRecord, SValue, SView, SyncSnapshot};
use std::collections::{HashMap, HashSet};

/// The id allocators the merge may draw on when it renumbers. The caller
/// (`app::state`) hands the session's own watermarks, so the fresh ids can
/// never collide with rows the session mints later.
pub struct MergeCtx<'a> {
    pub next_page: &'a mut dyn FnMut() -> u64,
    pub next_block: &'a mut dyn FnMut() -> u64,
    pub next_attachment: &'a mut dyn FnMut() -> u64,
    pub next_db: &'a mut dyn FnMut() -> u64,
    pub next_property: &'a mut dyn FnMut() -> u64,
    pub next_record: &'a mut dyn FnMut() -> u64,
    pub next_view: &'a mut dyn FnMut() -> u64,
}

#[derive(Debug, Default)]
pub struct MergeOutcome {
    pub merged: SyncSnapshot,
    /// One human-readable line per row the merge could not reconcile.
    pub conflicts: Vec<String>,
    /// (remote attachment id, local id it became) — the caller fetched the
    /// bytes under the remote id and must store them under the local one.
    pub attachment_remap: Vec<(u64, u64)>,
}

/// One row decision per key.
enum Pick {
    /// local's row (or absence) wins unchanged
    Local,
    /// remote's row (or absence) wins
    Remote,
    /// remote's row wins but its id collides with a different local row:
    /// give it a fresh id and keep both
    Renumber,
    /// both sides edited; local wins and the user is told
    Conflict,
}

fn id_index<T, K: Copy + Eq + std::hash::Hash>(
    rows: &[T],
    id_of: impl Fn(&T) -> K,
) -> HashMap<K, usize> {
    rows.iter().enumerate().map(|(i, r)| (id_of(r), i)).collect()
}

/// `s` is three-valued: `None` = no shadow at all (a first sync),
/// `Some(None)` = a shadow exists but never held this key, `Some(Some(row))` =
/// the row the two sides last agreed on.
fn decide<T: PartialEq>(l: Option<&T>, r: Option<&T>, s: Option<Option<&T>>) -> Pick {
    match (l, r) {
        (Some(lv), Some(rv)) if lv == rv => Pick::Local,
        (None, None) => Pick::Local,
        (Some(_), None) | (None, Some(_)) => match s {
            // the row existed at last sync and one side alone moved
            Some(Some(sv)) if l == Some(sv) => Pick::Remote,
            Some(Some(sv)) if r == Some(sv) => Pick::Local,
            Some(Some(_)) => Pick::Conflict,
            // no shadow row and only one side has the row: it was created
            // there since the last sync
            None | Some(None) => {
                if l.is_none() {
                    Pick::Remote
                } else {
                    Pick::Local
                }
            }
        },
        (Some(_), Some(_)) => match s {
            Some(Some(sv)) if l == Some(sv) => Pick::Remote,
            Some(Some(sv)) if r == Some(sv) => Pick::Local,
            Some(Some(_)) => Pick::Conflict,
            // both minted this id since the last sync and they disagree:
            // never silently drop either
            None | Some(None) => Pick::Renumber,
        },
    }
}

/// Flat three-way merge of one id-keyed collection. Returns the merged rows
/// each tagged with where they came from (true = the remote side's row was
/// taken), plus the remote rows that need fresh ids — a row's origin cannot
/// be read off its id, because a colliding id is exactly the case this
/// function exists to handle. The key is the row's identity within its
/// collection (a plain id, or the (record, property) pair a cell is named by).
fn merge_flat<T: Clone + PartialEq, K: Copy + Eq + std::hash::Hash + std::cmp::Ord + std::fmt::Debug>(
    local: &[T],
    shadow: Option<&[T]>,
    remote: &[T],
    id_of: impl Fn(&T) -> K,
    what: &str,
    peer: &str,
    conflicts: &mut Vec<String>,
    renumbered: &mut Vec<(K, T)>,
) -> Vec<(bool, T)> {
    let li = id_index(local, &id_of);
    let ri = id_index(remote, &id_of);
    let si = shadow.map(|s| id_index(s, &id_of));
    let mut keys: Vec<K> = li.keys().copied().chain(ri.keys().copied()).collect();
    keys.sort();
    keys.dedup();

    let mut out = Vec::new();
    for key in keys {
        let l = li.get(&key).map(|&i| &local[i]);
        let r = ri.get(&key).map(|&i| &remote[i]);
        let s = si.as_ref().map(|m| m.get(&key).map(|&i| &shadow.unwrap()[i]));
        match decide(l, r, s) {
            Pick::Local => {
                if let Some(lv) = l {
                    out.push((false, lv.clone()));
                }
            }
            Pick::Remote => {
                if let Some(rv) = r {
                    out.push((true, rv.clone()));
                }
                // an absent remote row is a delete: the row simply does not
                // join the output, and the caller reads the deletion off the
                // difference between its local state and the merged snapshot
            }
            Pick::Renumber => {
                // keep both: the local row stays where it is, and the remote
                // row is handed back to be given a fresh id
                if let Some(lv) = l {
                    out.push((false, lv.clone()));
                }
                if let Some(rv) = r {
                    renumbered.push((key, rv.clone()));
                }
            }
            Pick::Conflict => {
                conflicts.push(format!(
                    "{what} {key:?}: changed on both sides — kept this device's copy ({peer} keeps its own)"
                ));
                if let Some(lv) = l {
                    out.push((false, lv.clone()));
                }
            }
        }
    }
    out
}

/// Split a merged collection into (local-kept rows, remote-taken rows) —
/// only the latter may have their cross-references remapped.
fn split_origin<T>(rows: Vec<(bool, T)>) -> (Vec<T>, Vec<T>) {
    let mut kept = Vec::new();
    let mut taken = Vec::new();
    for (from_remote, row) in rows {
        if from_remote {
            taken.push(row);
        } else {
            kept.push(row);
        }
    }
    (kept, taken)
}

pub fn merge(
    local: &SyncSnapshot,
    shadow: Option<&SyncSnapshot>,
    remote: &SyncSnapshot,
    peer: &str,
    ctx: &mut MergeCtx,
) -> MergeOutcome {
    let mut conflicts: Vec<String> = Vec::new();
    let mut outcome = MergeOutcome::default();

    // ── the flat collections ──
    // `merge_flat` hands back origin-tagged rows and, separately, the remote
    // rows that collide and need fresh ids. Only remote-origin rows ever get
    // their cross-references remapped, which is why the origin travels with
    // every row instead of being guessed from its id.
    let mut q_pages: Vec<(u64, SPage)> = Vec::new();
    let (kept_pages, mut taken_pages) = split_origin(merge_flat(
        &local.pages,
        shadow.map(|s| s.pages.as_slice()),
        &remote.pages,
        |p| p.id,
        "page",
        peer,
        &mut conflicts,
        &mut q_pages,
    ));
    let mut renumber_pages = unwrap_pairs(q_pages);

    let mut q_atts: Vec<(u64, SAttachment)> = Vec::new();
    let (kept_atts, taken_atts) = split_origin(merge_flat(
        &local.attachments,
        shadow.map(|s| s.attachments.as_slice()),
        &remote.attachments,
        |a| a.id,
        "attachment",
        peer,
        &mut conflicts,
        &mut q_atts,
    ));
    let mut renumber_atts = unwrap_pairs(q_atts);

    let mut q_blocks: Vec<(u64, SBlock)> = Vec::new();
    let (kept_blocks, mut taken_blocks) = split_origin(merge_flat(
        &local.blocks,
        shadow.map(|s| s.blocks.as_slice()),
        &remote.blocks,
        |b| b.id,
        "block",
        peer,
        &mut conflicts,
        &mut q_blocks,
    ));
    let mut renumber_blocks = unwrap_pairs(q_blocks);

    // ── databases: the entity rows are compared as entities (id + name +
    // template) — their columns, views, records and cells travel flat below
    // and are grouped back onto whichever entity row wins. Comparing the
    // nested rows here would read "a cell changed" as "the database
    // changed" and log a conflict nobody can act on. ──
    let local_db_entities: Vec<SDatabase> = local.databases.iter().map(db_entity).collect();
    let remote_db_entities: Vec<SDatabase> = remote.databases.iter().map(db_entity).collect();
    let shadow_db_entities: Option<Vec<SDatabase>> =
        shadow.map(|s| s.databases.iter().map(db_entity).collect());
    let mut q_dbs: Vec<(u64, SDatabase)> = Vec::new();
    let (kept_dbs, taken_dbs) = split_origin(merge_flat(
        &local_db_entities,
        shadow_db_entities.as_deref(),
        &remote_db_entities,
        |d| d.id,
        "database",
        peer,
        &mut conflicts,
        &mut q_dbs,
    ));
    let mut renumber_dbs = unwrap_pairs(q_dbs);

    let (local_props, local_views, local_records, local_values) = flatten_dbs(&local.databases);
    let (remote_props, remote_views, remote_records, remote_values) = flatten_dbs(&remote.databases);
    let (shadow_props, shadow_views, shadow_records, shadow_values) = match shadow {
        Some(s) => flatten_dbs(&s.databases),
        None => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
    };

    let mut q_props: Vec<(u64, SProperty)> = Vec::new();
    let (kept_props, mut taken_props) = split_origin(merge_flat(
        &local_props,
        shadow.is_some().then_some(shadow_props.as_slice()),
        &remote_props,
        |p| p.id,
        "column",
        peer,
        &mut conflicts,
        &mut q_props,
    ));
    let mut renumber_props = unwrap_pairs(q_props);

    let mut q_views: Vec<(u64, SView)> = Vec::new();
    let (kept_views, mut taken_views) = split_origin(merge_flat(
        &local_views,
        shadow.is_some().then_some(shadow_views.as_slice()),
        &remote_views,
        |v| v.id,
        "view",
        peer,
        &mut conflicts,
        &mut q_views,
    ));
    let mut renumber_views = unwrap_pairs(q_views);

    let mut q_records: Vec<(u64, SRecord)> = Vec::new();
    let (kept_records, mut taken_records) = split_origin(merge_flat(
        &local_records,
        shadow.is_some().then_some(shadow_records.as_slice()),
        &remote_records,
        |r| r.id,
        "record",
        peer,
        &mut conflicts,
        &mut q_records,
    ));
    let mut renumber_records = unwrap_pairs(q_records);

    let mut q_values: Vec<((u64, u64), SValue)> = Vec::new();
    let (kept_values, mut taken_values) = split_origin(merge_flat(
        &local_values,
        shadow.is_some().then_some(shadow_values.as_slice()),
        &remote_values,
        |v| (v.record, v.property),
        "cell",
        peer,
        &mut conflicts,
        &mut q_values,
    ));
    let mut renumber_values: Vec<SValue> = unwrap_pairs(q_values);

    // ── cascades: a renumbered container pulls its remote rows along ──
    let mut out_block_ids: HashSet<u64> = taken_blocks.iter().map(|b| b.id).collect();
    for b in &renumber_blocks {
        out_block_ids.insert(b.id);
    }
    let page_ids: HashSet<u64> = renumber_pages.iter().map(|p| p.id).collect();
    for b in &remote.blocks {
        // `insert` answers "was this id not already in the output"; the local
        // half of the page's rows is untouched by a remote renumber
        if page_ids.contains(&b.page) && out_block_ids.insert(b.id) {
            renumber_blocks.push(b.clone());
        }
    }
    let db_ids: HashSet<u64> = renumber_dbs.iter().map(|d| d.id).collect();
    for p in &remote_props {
        if db_ids.contains(&p.db)
            && !taken_props.iter().any(|x| x.id == p.id)
            && !renumber_props.iter().any(|x| x.id == p.id)
        {
            renumber_props.push(p.clone());
        }
    }
    for v in &remote_views {
        if db_ids.contains(&v.db)
            && !taken_views.iter().any(|x| x.id == v.id)
            && !renumber_views.iter().any(|x| x.id == v.id)
        {
            renumber_views.push(v.clone());
        }
    }
    for r in &remote_records {
        if db_ids.contains(&r.db)
            && !taken_records.iter().any(|x| x.id == r.id)
            && !renumber_records.iter().any(|x| x.id == r.id)
        {
            renumber_records.push(r.clone());
        }
    }
    let record_ids: HashSet<u64> = renumber_records.iter().map(|r| r.id).collect();
    for v in &remote_values {
        let out = taken_values
            .iter()
            .chain(renumber_values.iter())
            .any(|x| x.record == v.record && x.property == v.property);
        if (db_ids.contains(&record_db(&remote_records, v.record)) || record_ids.contains(&v.record))
            && !out
        {
            renumber_values.push(v.clone());
        }
    }
    // renumbered blocks pull their attachments
    for b in &renumber_blocks {
        if let Some(att) = b.attachment {
            if !taken_atts.iter().any(|a| a.id == att) && !renumber_atts.iter().any(|a| a.id == att) {
                if let Some(ra) = remote.attachments.iter().find(|a| a.id == att) {
                    renumber_atts.push(ra.clone());
                }
            }
        }
    }

    // ── allocate fresh ids and build the remap tables ──
    let mut page_map: HashMap<u64, u64> = HashMap::new();
    let mut block_map: HashMap<u64, u64> = HashMap::new();
    let mut att_map: HashMap<u64, u64> = HashMap::new();
    let mut db_map: HashMap<u64, u64> = HashMap::new();
    let mut prop_map: HashMap<u64, u64> = HashMap::new();
    let mut record_map: HashMap<u64, u64> = HashMap::new();
    let mut view_map: HashMap<u64, u64> = HashMap::new();

    for p in &mut renumber_pages {
        let new = (ctx.next_page)();
        page_map.insert(p.id, new);
        p.id = new;
    }
    for a in &mut renumber_atts {
        let new = (ctx.next_attachment)();
        att_map.insert(a.id, new);
        outcome.attachment_remap.push((a.id, new));
        a.id = new;
    }
    for b in &mut renumber_blocks {
        let new = (ctx.next_block)();
        block_map.insert(b.id, new);
        b.id = new;
    }
    for d in &mut renumber_dbs {
        let new = (ctx.next_db)();
        db_map.insert(d.id, new);
        d.id = new;
    }
    for p in &mut renumber_props {
        let new = (ctx.next_property)();
        prop_map.insert(p.id, new);
        p.id = new;
    }
    for r in &mut renumber_records {
        let new = (ctx.next_record)();
        record_map.insert(r.id, new);
        r.id = new;
    }
    for v in &mut renumber_views {
        let new = (ctx.next_view)();
        view_map.insert(v.id, new);
        v.id = new;
    }
    for v in &mut renumber_values {
        v.record = *record_map.get(&v.record).unwrap_or(&v.record);
        v.property = *prop_map.get(&v.property).unwrap_or(&v.property);
    }

    // ── remap cross-references on the remote-origin rows only ──
    for b in taken_blocks.iter_mut().chain(renumber_blocks.iter_mut()) {
        b.page = *page_map.get(&b.page).unwrap_or(&b.page);
        b.parent = b.parent.map(|v| *block_map.get(&v).unwrap_or(&v));
        b.page_ref = b.page_ref.map(|v| *page_map.get(&v).unwrap_or(&v));
        b.sync_ref = b.sync_ref.map(|v| *block_map.get(&v).unwrap_or(&v));
        b.attachment = b.attachment.map(|v| *att_map.get(&v).unwrap_or(&v));
        b.db_ref = b.db_ref.map(|v| *db_map.get(&v).unwrap_or(&v));
    }
    for p in taken_pages.iter_mut().chain(renumber_pages.iter_mut()) {
        p.parent = p.parent.map(|v| *page_map.get(&v).unwrap_or(&v));
        p.cover = p.cover.map(|v| *att_map.get(&v).unwrap_or(&v));
    }
    for p in taken_props.iter_mut().chain(renumber_props.iter_mut()) {
        p.db = *db_map.get(&p.db).unwrap_or(&p.db);
    }
    for v in taken_views.iter_mut().chain(renumber_views.iter_mut()) {
        v.db = *db_map.get(&v.db).unwrap_or(&v.db);
    }
    for r in taken_records.iter_mut().chain(renumber_records.iter_mut()) {
        r.db = *db_map.get(&r.db).unwrap_or(&r.db);
        r.page = r.page.map(|v| *page_map.get(&v).unwrap_or(&v));
    }
    for v in taken_values.iter_mut().chain(renumber_values.iter_mut()) {
        v.record = *record_map.get(&v.record).unwrap_or(&v.record);
        v.property = *prop_map.get(&v.property).unwrap_or(&v.property);
    }

    // ── assemble: rows grouped back into their database entries ──
    let all_props: Vec<SProperty> = kept_props
        .into_iter()
        .chain(taken_props)
        .chain(renumber_props)
        .collect();
    let all_views: Vec<SView> = kept_views
        .into_iter()
        .chain(taken_views)
        .chain(renumber_views)
        .collect();
    let all_records: Vec<SRecord> = kept_records
        .into_iter()
        .chain(taken_records)
        .chain(renumber_records)
        .collect();
    let all_values: Vec<SValue> = kept_values
        .into_iter()
        .chain(taken_values)
        .chain(renumber_values)
        .collect();

    let mut all_dbs: Vec<SDatabase> = kept_dbs.into_iter().chain(taken_dbs).chain(renumber_dbs).collect();
    for d in all_dbs.iter_mut() {
        d.properties = all_props.iter().filter(|p| p.db == d.id).cloned().collect();
        d.views = all_views.iter().filter(|v| v.db == d.id).cloned().collect();
        d.records = all_records.iter().filter(|r| r.db == d.id).cloned().collect();
        d.values = all_values
            .iter()
            .filter(|v| d.records.iter().any(|r| r.id == v.record))
            .cloned()
            .collect();
    }

    // parents before children so an apply can insert in list order
    let mut pages: Vec<SPage> = kept_pages.into_iter().chain(taken_pages).chain(renumber_pages).collect();
    let mut blocks: Vec<SBlock> =
        kept_blocks.into_iter().chain(taken_blocks).chain(renumber_blocks).collect();
    let attachments: Vec<SAttachment> = kept_atts.into_iter().chain(taken_atts).chain(renumber_atts).collect();
    sort_pages_parents_first(&mut pages);
    sort_blocks_parents_first(&mut blocks);

    outcome.merged = SyncSnapshot {
        version: remote.version,
        // the merged content is this device's to push back: `app::state`
        // stamps its own identity over these two before storing or sending
        device_id: remote.device_id.clone(),
        device: remote.device.clone(),
        pages,
        blocks,
        attachments,
        databases: all_dbs,
    };
    outcome.conflicts = conflicts;
    outcome
}

/// Drop the keys `merge_flat` bubbled up alongside its renumber candidates.
fn unwrap_pairs<K, T>(pairs: Vec<(K, T)>) -> Vec<T> {
    pairs.into_iter().map(|(_, row)| row).collect()
}

fn record_db(records: &[SRecord], id: u64) -> u64 {
    records
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.db)
        .unwrap_or(0)
}

/// A database row with its nested rows stripped: what the entity-level merge
/// compares and what the assembly step re-fills from the flat lists.
fn db_entity(d: &SDatabase) -> SDatabase {
    SDatabase {
        id: d.id,
        name: d.name.clone(),
        template: d.template.clone(),
        properties: Vec::new(),
        views: Vec::new(),
        records: Vec::new(),
        values: Vec::new(),
    }
}

fn flatten_dbs(
    dbs: &[SDatabase],
) -> (
    Vec<SProperty>,
    Vec<SView>,
    Vec<SRecord>,
    Vec<SValue>,
) {
    let mut props = Vec::new();
    let mut views = Vec::new();
    let mut records = Vec::new();
    let mut values = Vec::new();
    for d in dbs {
        props.extend(d.properties.iter().cloned());
        views.extend(d.views.iter().cloned());
        records.extend(d.records.iter().cloned());
        values.extend(d.values.iter().cloned());
    }
    (props, views, records, values)
}

/// Parents first, ties by id, so an apply can create rows in list order.
pub fn sort_pages_parents_first(pages: &mut [SPage]) {
    let by_id: HashMap<u64, SPage> = pages.iter().map(|p| (p.id, p.clone())).collect();
    let depth = |start: u64| -> usize {
        let mut depth = 0;
        let mut cur = Some(start);
        let mut seen = HashSet::new();
        while let Some(id) = cur {
            if !seen.insert(id) {
                break; // a cycle is the bulk path's acyclicity check's problem
            }
            match by_id.get(&id).and_then(|p| p.parent) {
                Some(parent) => {
                    depth += 1;
                    cur = Some(parent);
                }
                None => break,
            }
        }
        depth
    };
    pages.sort_by_cached_key(|p| (depth(p.id), p.id));
}

/// Container rows before the rows they contain, ties by id.
pub fn sort_blocks_parents_first(blocks: &mut [SBlock]) {
    let by_id: HashMap<u64, SBlock> =
        blocks.iter().map(|b| (b.id, b.clone())).collect();
    let depth = |start: u64| -> usize {
        let mut depth = 0;
        let mut cur = Some(start);
        let mut seen = HashSet::new();
        while let Some(id) = cur {
            if !seen.insert(id) {
                break;
            }
            match by_id.get(&id).and_then(|b| b.parent) {
                Some(parent) => {
                    depth += 1;
                    cur = Some(parent);
                }
                None => break,
            }
        }
        depth
    };
    blocks.sort_by_cached_key(|b| (depth(b.id), b.id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> SyncSnapshot {
        SyncSnapshot::default()
    }

    fn page(id: u64, parent: Option<u64>) -> SPage {
        SPage {
            id,
            title: format!("p{id}"),
            parent,
            ord: id,
            favorite: false,
            expanded: false,
            font: String::new(),
            full_width: false,
            small_text: false,
            icon: String::new(),
            cover: None,
            locked: false,
            template: false,
        }
    }

    fn block(id: u64, page: u64) -> SBlock {
        SBlock {
            id,
            page,
            parent: None,
            ord: id,
            kind: "para".into(),
            text: format!("b{id}"),
            checked: false,
            folded: false,
            color: String::new(),
            background: String::new(),
            page_ref: None,
            sync_ref: None,
            attachment: None,
            img_percent: 100,
            columns: 0,
            lang: String::new(),
            db_ref: None,
            marks: Vec::new(),
        }
    }

    /// One shared counter for every namespace: fresh ids only need to be
    /// unique, and a single pool makes each test's numbers readable. The
    /// closures cannot outlive this frame, so the merge call runs inside
    /// `with_ctx`.
    fn with_ctx<T>(start: u64, f: impl FnOnce(&mut MergeCtx<'_>) -> T) -> T {
        let c = std::rc::Rc::new(std::cell::Cell::new(start));
        fn alloc(c: &std::rc::Rc<std::cell::Cell<u64>>) -> u64 {
            let next = c.get() + 1;
            c.set(next);
            next
        }
        let mut page = { let c = c.clone(); move || alloc(&c) };
        let mut block = { let c = c.clone(); move || alloc(&c) };
        let mut att = { let c = c.clone(); move || alloc(&c) };
        let mut db = { let c = c.clone(); move || alloc(&c) };
        let mut prop = { let c = c.clone(); move || alloc(&c) };
        let mut rec = { let c = c.clone(); move || alloc(&c) };
        let mut view = { let c = c.clone(); move || alloc(&c) };
        let mut ctx = MergeCtx {
            next_page: &mut page,
            next_block: &mut block,
            next_attachment: &mut att,
            next_db: &mut db,
            next_property: &mut prop,
            next_record: &mut rec,
            next_view: &mut view,
        };
        f(&mut ctx)
    }

    #[test]
    fn remote_only_page_is_taken() {
        let mut local = snap();
        local.pages.push(page(1, None));
        let mut remote = snap();
        remote.pages.push(page(1, None));
        remote.pages.push(page(2, Some(1)));

        let out = with_ctx(1000, |ctx| merge(&local, None, &remote, "phone", ctx));
        assert_eq!(out.merged.pages.len(), 2);
        assert!(out.merged.pages.iter().any(|p| p.id == 2));
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn local_only_edit_survives_and_remote_delete_lands() {
        let mut base = snap();
        let mut p = page(1, None);
        p.title = "shared".into();
        base.pages.push(p);
        let shadow = base.clone();

        // a remote delete over an unchanged local copy lands
        let out = with_ctx(0, |ctx| merge(&base, Some(&shadow), &snap(), "phone", ctx));
        assert!(out.merged.pages.is_empty());

        // …but a local edit over an unchanged remote copy keeps the row
        let mut local = snap();
        let mut lp = page(1, None);
        lp.title = "edited here".into();
        local.pages.push(lp);
        let out = with_ctx(0, |ctx| merge(&local, Some(&shadow), &base, "phone", ctx));
        assert_eq!(out.merged.pages.len(), 1);
        assert_eq!(out.merged.pages[0].title, "edited here");
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);

        // and a local edit against a remote DELETE is a conflict that keeps
        // the edit (the peer keeps its own answer) and says so
        let out = with_ctx(0, |ctx| merge(&local, Some(&shadow), &snap(), "phone", ctx));
        assert_eq!(out.merged.pages.len(), 1);
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    }

    #[test]
    fn both_edits_conflict_and_are_logged() {
        let mut base = snap();
        let mut p = page(1, None);
        p.title = "shared".into();
        base.pages.push(p);
        let shadow = base.clone();

        let mut local = snap();
        let mut lp = page(1, None);
        lp.title = "here".into();
        local.pages.push(lp);

        let mut remote = snap();
        let mut rp = page(1, None);
        rp.title = "there".into();
        remote.pages.push(rp);

        let out = with_ctx(0, |ctx| merge(&local, Some(&shadow), &remote, "phone", ctx));
        assert_eq!(out.merged.pages.len(), 1);
        assert_eq!(out.merged.pages[0].title, "here");
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    }

    #[test]
    fn colliding_new_ids_renumber_and_keep_both() {
        // both devices minted page 7 since the last sync
        let mut local = snap();
        let mut lp = page(7, None);
        lp.title = "local new".into();
        local.pages.push(lp);
        local.blocks.push(block(70, 7));

        let mut remote = snap();
        let mut rp = page(7, None);
        rp.title = "remote new".into();
        remote.pages.push(rp);
        remote.blocks.push(block(70, 7));

        let c = 1000u64;
        let out = with_ctx(c, |ctx| merge(&local, Some(&snap()), &remote, "phone", ctx));
        assert!(
            out.merged
                .pages
                .iter()
                .any(|p| p.id == 7 && p.title == "local new")
        );
        let twin = out
            .merged
            .pages
            .iter()
            .find(|p| p.id != 7 && p.title == "remote new")
            .expect("remote page renumbered alongside");
        let twin_block = out
            .merged
            .blocks
            .iter()
            .find(|b| b.page == twin.id)
            .expect("its block came along");
        assert!(twin_block.id != 70, "the block was renumbered too");
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn renumbered_blocks_remap_their_page() {
        let mut local = snap();
        local.pages.push(page(7, None));
        local.blocks.push(block(70, 7));

        let mut remote = snap();
        let mut rp = page(7, None);
        rp.title = "different".into();
        remote.pages.push(rp);
        let mut rb = block(70, 7);
        rb.text = "different".into();
        remote.blocks.push(rb);

        let out = with_ctx(1000, |ctx| merge(&local, Some(&snap()), &remote, "phone", ctx));
        // local page and its block stay; the remote pair is renumbered and
        // the renumbered block points at the renumbered page
        assert!(out.merged.blocks.iter().any(|b| b.id == 70 && b.page == 7));
        let twin_block = out.merged.blocks.iter().find(|b| b.id != 70).expect("remote block renumbered");
        assert!(out.merged.pages.iter().any(|p| p.id == twin_block.page));
    }

    #[test]
    fn parents_sort_before_children() {
        let mut pages: Vec<SPage> = vec![page(3, Some(2)), page(2, Some(1)), page(1, None)];
        sort_pages_parents_first(&mut pages);
        let ids: Vec<u64> = pages.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);

        let mut blocks: Vec<SBlock> = vec![
            {
                let mut b = block(3, 1);
                b.parent = Some(2);
                b
            },
            {
                let mut b = block(2, 1);
                b.parent = Some(1);
                b
            },
            block(1, 1),
        ];
        sort_blocks_parents_first(&mut blocks);
        let ids: Vec<u64> = blocks.iter().map(|b| b.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn both_edited_cells_conflict_and_log() {
        let mut base = snap();
        base.databases.push(SDatabase {
            id: 1,
            name: "db".into(),
            template: String::new(),
            properties: vec![SProperty {
                id: 10,
                db: 1,
                name: "Name".into(),
                kind: "title".into(),
                config: String::new(),
                ord: 0,
            }],
            views: vec![],
            records: vec![SRecord { id: 20, db: 1, page: None, ord: 1 }],
            values: vec![SValue {
                record: 20,
                property: 10,
                text: Some("hello".into()),
                num: None,
                flag: None,
                items: None,
            }],
        });
        let shadow = base.clone();

        let mut local = base.clone();
        local.databases[0].values[0].text = Some("edited here".into());

        let mut remote = base.clone();
        remote.databases[0].values[0].text = Some("edited there".into());

        let out = with_ctx(0, |ctx| merge(&local, Some(&shadow), &remote, "phone", ctx));
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(
            out.merged.databases[0].values[0].text.as_deref(),
            Some("edited here")
        );
    }
}
