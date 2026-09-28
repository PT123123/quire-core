// The three-way merge, with **two keys**: the flat collections (pages, blocks,
// databases, attachments, lists) are keyed by their integer id, and SPEC §四十一's
// notes and tasks by their 唯一 ID — `aw-server-plus`'s "uuid 逻辑键 + rev 仲裁"
// applied where the ids to do it with exist.
//
// Inputs: the local snapshot, the shadow (what the two peers last agreed on —
// `app::state` persists one per peer), and the remote snapshot. Output: the
// merged snapshot plus what the user should be told.
//
// **The id-keyed half.** Per row of every flat collection, over the union of the
// three key sets:
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
// **The uuid-keyed half** (`merge_organizer`). A note and a task carry the 唯一 ID
// the area mints *precisely because* an integer id cannot serve as one — it is a
// per-device watermark — so they are merged by that id, and the id collision the
// flat pass answers by renumbering simply cannot arise. Two copies of one row are
// then ordered by the row's **revision** (`core::organizer::rev`: `(millis,
// device)` compared as a string); the newer copy stands, and both peers compute
// that answer from the same two strings — so a concurrent edit converges in one
// round instead of being re-decided, silently and the other way, by the next.
// The shadow keeps the one job the revision cannot do: it says whether a uuid
// missing from one side is a row that side never had (a new row) or one it
// **purged**.
//
// A first sync between two devices that both have content runs with no shadow.
// The id-keyed half then reads every differing row as a conflict and keeps local:
// the honest answer, because two workspaces merged for the first time are one
// user decision, not one algorithm — and the rows that only one side has still
// flow. The uuid-keyed half takes the newer copy of each row, which is the best
// answer available without asking and the one `aw-server-plus` gives.
//
// Renumbering no longer touches the organizer at all: a note's parent
// (`ref_note`) and a task's list are remapped through the maps the two passes
// built, because a remote row's pointers are remote ids whatever key the row
// itself was matched by.

use super::model::{
    SAttachment, SBlock, SDatabase, SPage, SProperty, SRecord, STaskList, SValue, SView,
    SyncSnapshot,
};
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
    /// SPEC §四十一's three. Notes and tasks are the two the uuid-keyed pass may
    /// have to **insert** — a remote row this device has never seen gets a fresh
    /// local id here, so the two devices' integer ids never have to agree. A
    /// renumbered subtask draws on the **task** allocator, because a subtask's id
    /// is scoped to its task and the two are never cross-referenced — and a
    /// subtask travels *inside* its task's row, so it is never allocated for
    /// alone.
    pub next_note: &'a mut dyn FnMut() -> u64,
    pub next_task: &'a mut dyn FnMut() -> u64,
    /// Lists keep the id-keyed pass (a list carries no uuid), so this is still the
    /// renumber allocator the flat collections use.
    pub next_list: &'a mut dyn FnMut() -> u64,
    /// Mints a 唯一 ID for an organizer row whose `uuid` arrived blank — a peer at
    /// the rev before the column, or a row neither side had backfilled. A
    /// closure and not an RNG owned here, for the reason every `next_*` is one:
    /// the caller's session owns the identities it mints.
    pub new_uuid: &'a mut dyn FnMut() -> String,
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

/// Index one collection by the uuid it carries. Two rows that somehow share a
/// uuid collapse to the last — a state `new_uuid` exists to make impossible, and
/// one the pass below must not panic on if a hand-edited file holds it.
fn uuid_index<T>(rows: &[T], uuid_of: impl for<'a> Fn(&'a T) -> &'a str) -> HashMap<String, usize> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| (uuid_of(r).to_string(), i))
        .collect()
}

/// Whether both sides moved this row since the last sync — the question that
/// decides whether a revision-arbitrated difference is worth a line in the log, or
/// is simply the ordinary "one side edited it" the revision resolves in silence.
fn both_moved<T: PartialEq>(shadow: Option<&T>, local: &T, remote: &T) -> bool {
    match shadow {
        Some(sv) => local != sv && remote != sv,
        None => false,
    }
}

/// The deterministic last resort when two revisions compare **equal**: order the
/// two copies by their own serialized contents.
///
/// Reachable only for the two rows a v30 backfill stamped from `edited`, which
/// carries no device half — so a tie means both were written in the same second on
/// devices that predate the revision column. Both ends hold the same two rows, so
/// both compute the same answer from them and the tie still converges; what it
/// gives up is any claim about which write came *second*, which the clock never
/// knew anyway.
fn tie_break(local: &str, remote: &str) -> bool {
    remote > local
}

/// Merge one of the organizer's two uuid-keyed collections (`notes`, `tasks`).
///
/// **Keyed by the 唯一 ID, not the row's integer id.** That is the whole
/// difference from the flat pass above, and the reason this function exists: an
/// integer id is a per-device `max + 1` watermark, two devices mint the same one
/// for *different* rows routinely, and the flat pass answers that by renumbering —
/// which is right for a page and wrong for a row whose identity is supposed to be
/// stable, because a renumbered row may already be the same row under a second id.
/// The uuid is minted to be unique (`core::organizer::new_uuid`), so keying on it
/// makes that class of collision impossible rather than papered over.
///
/// **Arbitrated by the revision** — `(instant, device)` compared as a string, the
/// newest copy wins. Both ends see the same two strings, so both pick the same
/// winner; a conflict therefore converges in the round that finds it instead of
/// being settled again, the other way, by the next.
///
/// The shadow still does the one job the revision cannot: for a uuid **missing**
/// from one side it says whether that side never had the row (a new row to insert)
/// or **purged** it (a removal to leave alone). A bin needs no such help — the
/// tombstone is on the row and travels with it.
///
/// Returns each merged row tagged with where it came from (`true` = the remote copy
/// was taken, so the caller still has to walk its cross-references through the id
/// maps), plus the `remote id → local id` map those maps are built from.
fn merge_organizer<T: Clone + PartialEq>(
    local: &[T],
    shadow: Option<&[T]>,
    remote: &[T],
    id_of: impl Fn(&T) -> u64,
    uuid_of: impl for<'a> Fn(&'a T) -> &'a str,
    rev_of: impl for<'a> Fn(&'a T) -> &'a str,
    set_uuid: impl for<'a> Fn(&'a mut T, String),
    set_id: impl Fn(&mut T, u64),
    content: impl Fn(&T) -> String,
    what: &str,
    peer: &str,
    conflicts: &mut Vec<String>,
    new_uuid: &mut dyn FnMut() -> String,
    next_id: &mut dyn FnMut() -> u64,
) -> (Vec<(bool, T)>, HashMap<u64, u64>) {
    // Identity first, so every row below has a key to be matched by. A blank uuid
    // is *no identity* rather than an empty one — the state a row written before
    // the column (or by a hand-edited file) arrives in — and it is named here for
    // the same reason the old pass named it: a row must not reach this device's
    // file without a name, and a peer's row without one must not be dropped.
    let mut local: Vec<T> = local.to_vec();
    let mut remote: Vec<T> = remote.to_vec();
    for r in local.iter_mut() {
        if uuid_of(r).is_empty() {
            let fresh = new_uuid();
            set_uuid(r, fresh);
        }
    }
    for r in remote.iter_mut() {
        if uuid_of(r).is_empty() {
            let fresh = new_uuid();
            set_uuid(r, fresh);
        }
    }
    let shadow_rows: Vec<T> = shadow.map(<[T]>::to_vec).unwrap_or_default();
    let has_shadow = shadow.is_some();

    let li = uuid_index(&local, &uuid_of);
    let ri = uuid_index(&remote, &uuid_of);
    let si = uuid_index(&shadow_rows, &uuid_of);

    let mut keys: Vec<&String> = li.keys().chain(ri.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut out: Vec<(bool, T)> = Vec::new();
    let mut id_map: HashMap<u64, u64> = HashMap::new();

    for key in keys {
        let l = li.get(key).map(|&i| local[i].clone());
        let r = ri.get(key).map(|&i| remote[i].clone());
        // Three-valued: `None` = no shadow at all (a first sync), `Some(None)` = a
        // shadow exists but never held this uuid, `Some(Some(row))` = the copy the
        // two sides last agreed on.
        let s: Option<Option<T>> = if has_shadow {
            Some(si.get(key).map(|&i| shadow_rows[i].clone()))
        } else {
            None
        };

        match (&l, &r) {
            (Some(lv), Some(rv)) => match lv == rv {
                true => out.push((false, lv.clone())),
                false => {
                    let take_remote = match rev_of(rv).cmp(rev_of(lv)) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Less => false,
                        std::cmp::Ordering::Equal => tie_break(&content(lv), &content(rv)),
                    };
                    let settled = both_moved(s.as_ref().and_then(|sv| sv.as_ref()), lv, rv);
                    if take_remote {
                        // The row already exists here under this device's id, so it
                        // is *rewritten* rather than deleted and re-inserted — the
                        // id stays put and only the content moves.
                        let mut taken = rv.clone();
                        let here = id_of(lv);
                        set_id(&mut taken, here);
                        id_map.insert(id_of(rv), here);
                        if settled {
                            conflicts.push(changed_on_both(what, key, peer, true));
                        }
                        out.push((true, taken));
                    } else {
                        id_map.insert(id_of(rv), id_of(lv));
                        if settled {
                            conflicts.push(changed_on_both(what, key, peer, false));
                        }
                        out.push((false, lv.clone()));
                    }
                }
            },
            // Only this side has it. The shadow says which of the two reasons.
            (Some(lv), None) => match &s {
                // Untouched here, gone there: the peer purged it. Stay purged.
                Some(Some(sv)) if sv == lv => {}
                // Edited here, purged there: a purge is not an operation this
                // protocol carries, so this device's copy — the row the user still
                // has — stands, and the disagreement is said out loud.
                Some(Some(_)) => {
                    conflicts.push(format!(
                        "{what} {}: 对端彻底删除，本机改过 — 保留本机的这一份",
                        short(key)
                    ));
                    out.push((false, lv.clone()));
                }
                // No shadow, or a shadow that never held it: a row made here since
                // the last sync. It simply travels.
                _ => out.push((false, lv.clone())),
            },
            // Only the peer has it, mirrored.
            (None, Some(rv)) => match &s {
                // Untouched there, gone here: this device purged it. Stay purged.
                Some(Some(sv)) if sv == rv => {}
                // Purged here, edited there: the purge stands, and it is said.
                Some(Some(_)) => {
                    conflicts.push(format!(
                        "{what} {}: 本机彻底删除，对端改过 — 保持删除",
                        short(key)
                    ));
                }
                // A row this device has never seen: insert it under a fresh local
                // id, so the two devices' watermarks never have to agree.
                _ => {
                    let mut fresh = rv.clone();
                    let new_id = next_id();
                    set_id(&mut fresh, new_id);
                    id_map.insert(id_of(rv), new_id);
                    out.push((true, fresh));
                }
            },
            (None, None) => {}
        }
    }

    (out, id_map)
}

/// The first eight characters of a uuid, for a log line a person reads.
fn short(uuid: &str) -> &str {
    let end = uuid.len().min(8);
    &uuid[..end]
}

/// One line for a row both sides moved. The revision decided it, and the line says
/// which copy stands — because the two devices' pages now agree and nothing else
/// on the 同步 page could explain why the other one's edit is not the one shown.
fn changed_on_both(what: &str, uuid: &str, peer: &str, took_remote: bool) -> String {
    if took_remote {
        format!(
            "{what} {}: 两边都改过 — 取了 {peer} 更新的一份",
            short(uuid)
        )
    } else {
        format!("{what} {}: 两边都改过 — 保留本机更新的一份", short(uuid))
    }
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

    // ── SPEC §四十一: the organizer's three collections ──
    //
    // Flat and independent of everything above — no page, no block, no database.
    // Notes and tasks go through the **uuid-keyed** pass (`merge_organizer`), which
    // is SPEC §四十一's 唯一 ID doing the job it was minted for; lists keep the
    // id-keyed pass above, because a list carries no uuid of its own. The two
    // pointers among the three are walked below, once both passes have run: a
    // task's list through the lists' renumbering map, a comment's parent through
    // the notes' `remote id → local id` map.
    let (mut note_rows, note_map) = merge_organizer(
        &local.notes,
        shadow.map(|s| s.notes.as_slice()),
        &remote.notes,
        |n| n.id,
        |n| n.uuid.as_str(),
        |n| n.rev.as_str(),
        |n, v| n.uuid = v,
        |n, v| n.id = v,
        |n| serde_json::to_string(n).unwrap_or_default(),
        "note",
        peer,
        &mut conflicts,
        &mut *ctx.new_uuid,
        &mut *ctx.next_note,
    );

    let mut q_lists: Vec<(u64, STaskList)> = Vec::new();
    let (kept_lists, taken_lists) = split_origin(merge_flat(
        &local.lists,
        shadow.map(|s| s.lists.as_slice()),
        &remote.lists,
        |l| l.id,
        "list",
        peer,
        &mut conflicts,
        &mut q_lists,
    ));
    let mut renumber_lists = unwrap_pairs(q_lists);

    let (mut task_rows, _task_map) = merge_organizer(
        &local.tasks,
        shadow.map(|s| s.tasks.as_slice()),
        &remote.tasks,
        |t| t.id,
        |t| t.uuid.as_str(),
        |t| t.rev.as_str(),
        |t, v| t.uuid = v,
        |t, v| t.id = v,
        |t| serde_json::to_string(t).unwrap_or_default(),
        "task",
        peer,
        &mut conflicts,
        &mut *ctx.new_uuid,
        &mut *ctx.next_task,
    );

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
    // Which local list a renumbered remote list became — the organizer's one
    // cross-reference (`STask.list`). A fresh id is allocated here rather than
    // extrapolated, so the remap below cannot accidentally land on 0, which is
    // the inbox and not a list.
    let mut list_map: HashMap<u64, u64> = HashMap::new();

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
    // The organizer's lists, allocated like every flat collection above. Notes and
    // tasks never reach this loop: the uuid-keyed pass already gave a fresh id to
    // each row it had to insert, and left the rows it matched under this device's
    // own id.
    for l in &mut renumber_lists {
        let new = (ctx.next_list)();
        list_map.insert(l.id, new);
        l.id = new;
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
    // A task's list, the one pointer in the organizer. Remote-origin rows only, for
    // the same reason as every remap above: a local row's list id is this device's
    // and the merge never moved it. `0` is not in the map (the inbox is not a row
    // and cannot be renumbered), so a task in the inbox stays in the inbox.
    for (from_remote, t) in task_rows.iter_mut() {
        if *from_remote {
            t.list = *list_map.get(&t.list).unwrap_or(&t.list);
        }
    }
    // A comment's parent, on remote-origin notes only for the same reason. The
    // `unwrap_or` is load-bearing and is the `page_ref` rule exactly: a ref to a
    // note that **did not survive** the merge is left alone rather than cleared, so
    // a comment whose parent is not here stays a comment — and if the parent
    // arrives in a later sync the ref is already right. What comes out of the
    // uuid-keyed pass is a *remote id → local id* map for every remote note that
    // landed (an inserted one under a fresh id, a rewritten one under this
    // device's), which is the map a comment written on the other device needs.
    for (from_remote, n) in note_rows.iter_mut() {
        if *from_remote {
            n.ref_note = n.ref_note.map(|v| *note_map.get(&v).unwrap_or(&v));
        }
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
        // SPEC §四十一: the uuid-keyed pass answers its rows already tagged with
        // origin, so the tag is dropped here; the order is local rows first, then
        // the remote ones that were taken or inserted, and none of it a constraint
        // on the app (it reads the snapshot into its own catalog, which sorts
        // itself).
        notes: note_rows.into_iter().map(|(_, row)| row).collect(),
        tasks: task_rows.into_iter().map(|(_, row)| row).collect(),
        lists: kept_lists
            .into_iter()
            .chain(taken_lists)
            .chain(renumber_lists)
            .collect(),
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
    use crate::services::sync::model::{SNote, SSubtask, STask};

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
        let mut note = { let c = c.clone(); move || alloc(&c) };
        let mut task = { let c = c.clone(); move || alloc(&c) };
        let mut list = { let c = c.clone(); move || alloc(&c) };
        // A test uuid is deterministic and shaped like a real one, so a merge
        // that mints for a blank row is still reproducible run to run.
        let seq = std::cell::Cell::new(0u64);
        let mut uuid = move || {
            let next = seq.get() + 1;
            seq.set(next);
            format!("{next:032x}")
        };
        let mut ctx = MergeCtx {
            next_page: &mut page,
            next_block: &mut block,
            next_attachment: &mut att,
            next_db: &mut db,
            next_property: &mut prop,
            next_record: &mut rec,
            next_view: &mut view,
            next_note: &mut note,
            next_task: &mut task,
            next_list: &mut list,
            new_uuid: &mut uuid,
        };
        f(&mut ctx)
    }

    fn note(id: u64, title: &str) -> SNote {
        SNote {
            id,
            uuid: format!("{id:032x}"),
            title: title.into(),
            body: format!("body of {title}"),
            pinned: false,
            tags: vec!["tag".into()],
            created: 1_700_000_000 + id as i64,
            edited: 1_700_000_900 + id as i64,
            ref_note: None,
            deleted_at: None,
            rev: crate::core::organizer::rev(1_700_000_000_000 + id as i64, "phone"),
        }
    }

    /// A reply: [`note`] with its ref set. Its own helper so a test that is about
    /// the ref does not have to build the whole row to say so.
    fn comment(id: u64, title: &str, parent: u64) -> SNote {
        SNote { ref_note: Some(parent), ..note(id, title) }
    }

    fn list(id: u64, name: &str) -> STaskList {
        STaskList {
            id,
            name: name.into(),
            color: "blue".into(),
            ord: id,
        }
    }

    fn task(id: u64, list: u64, title: &str) -> STask {
        STask {
            id,
            uuid: format!("{:032x}", 1_000 + id),
            list,
            title: title.into(),
            notes: String::new(),
            priority: "medium".into(),
            due: Some("2026-09-24".into()),
            repeat: "weekly".into(),
            done: false,
            completed_at: None,
            tags: vec!["work".into()],
            subtasks: vec![SSubtask {
                id: id * 10,
                title: "step".into(),
                done: false,
            }],
            created: 1_700_000_000 + id as i64,
            edited: 1_700_000_900 + id as i64,
            ord: id,
            deleted_at: None,
            rev: crate::core::organizer::rev(1_700_000_000_000 + id as i64, "phone"),
        }
    }

    /// One **write** of a note, as the app layer makes one: the content moves *and*
    /// the revision is restamped. A fixture that changed only the content would
    /// describe a row no shell can produce — every organizer write goes through the
    /// funnel that stamps it — and would leave the merge arbitrating a tie.
    fn note_at(mut n: SNote, millis: i64, device: &str, edit: impl FnOnce(&mut SNote)) -> SNote {
        edit(&mut n);
        n.rev = crate::core::organizer::rev(millis, device);
        n
    }

    /// [`note_at`], one entity over.
    fn task_at(mut t: STask, millis: i64, device: &str, edit: impl FnOnce(&mut STask)) -> STask {
        edit(&mut t);
        t.rev = crate::core::organizer::rev(millis, device);
        t
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

    // ─── SPEC §四十一: the organizer's three collections ───────────────────

    /// The organizer is three flat collections beside the pages and blocks: a
    /// row only one side has flows across, a row both sides already agree on
    /// stays put, and a task in the inbox needs no list row to arrive with it —
    /// `0` is the sentinel, so the inbox costs the wire nothing.
    #[test]
    fn the_organizer_flows_flat_and_a_remote_only_row_is_taken() {
        let mut local = snap();
        local.notes.push(note(1, "mine"));
        local.lists.push(list(2, "Mine"));
        local.tasks.push(task(3, 2, "mine"));

        let mut remote = snap();
        remote.notes.push(note(1, "mine")); // converged
        remote.notes.push(note(4, "theirs"));
        remote.lists.push(list(2, "Mine"));
        remote.lists.push(list(5, "Theirs"));
        remote.tasks.push(task(3, 2, "mine"));
        remote.tasks.push(task(6, 5, "theirs"));
        remote.tasks.push(task(7, 0, "in the inbox"));

        let out = with_ctx(1000, |ctx| merge(&local, None, &remote, "phone", ctx));
        assert_eq!(out.merged.notes.len(), 2);
        assert_eq!(out.merged.lists.len(), 2);
        assert_eq!(out.merged.tasks.len(), 3);
        assert!(out.merged.notes.iter().any(|n| n.title == "theirs"));
        assert!(out.merged.lists.iter().any(|l| l.name == "Theirs"));
        assert!(out
            .merged
            .tasks
            .iter()
            .any(|t| t.title == "in the inbox" && t.list == 0));
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
    }

    /// SPEC §四十一's 唯一 ID is the merge's **key** for a note or a task, and that
    /// is the whole of what separates the organizer's two collections from the flat
    /// ones above: two devices that hand the same integer to *different* rows are
    /// not a collision to be renumbered around — they are two rows — and two devices
    /// that hold the **same** uuid hold the same row whatever integers they filed it
    /// under.
    ///
    /// A blank uuid is no identity at all rather than an empty one, so it is minted
    /// here: a row must not reach a peer without a name, and a peer's unnamed row
    /// must not be dropped.
    #[test]
    fn the_uuid_is_the_key_not_the_integer_id() {
        // Same uuid, different integers: one row, and it keeps this device's id.
        let mut local = snap();
        local.notes.push(note(7, "one row, two integers"));
        local.notes[0].uuid = "a".repeat(32);

        let mut remote = snap();
        remote.notes.push(note(9, "one row, two integers"));
        remote.notes[0].uuid = "a".repeat(32);

        let out = with_ctx(1000, |ctx| merge(&local, None, &remote, "phone", ctx));
        assert_eq!(out.merged.notes.len(), 1, "one uuid is one row");
        assert_eq!(out.merged.notes[0].id, 7, "and it keeps this device's integer");
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);

        // Same integer, different uuids: two rows, and neither is renumbered — an
        // integer is not an identity, so there is nothing to collide about.
        let mut local = snap();
        local.notes.push(note(3, "mine"));
        local.notes[0].uuid = "b".repeat(32);
        let mut remote = snap();
        remote.notes.push(note(3, "theirs"));
        remote.notes[0].uuid = "c".repeat(32);

        let out = with_ctx(1000, |ctx| merge(&local, None, &remote, "phone", ctx));
        assert_eq!(out.merged.notes.len(), 2, "same integer, different rows");
        assert_eq!(out.merged.notes[0].id, 3, "the local row is untouched");
        assert_ne!(out.merged.notes[1].id, 3, "the arrival got an integer of its own");
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);

        // A blank uuid is named here, 32 hex characters, and never matched against
        // another blank one.
        let mut local = snap();
        local.notes.push(note(1, "never named"));
        local.notes[0].uuid = String::new();
        let out = with_ctx(1000, |ctx| merge(&local, None, &snap(), "phone", ctx));
        let minted = &out.merged.notes[0].uuid;
        assert_eq!(minted.len(), 32, "{minted:?}");
        assert!(minted.chars().all(|c| c.is_ascii_hexdigit()), "{minted:?}");
    }

    /// The two decisions the revision and the shadow make together, asked of the
    /// organizer's rows: one side moving a row lands it, and both sides moving it is
    /// settled by the **newer revision** — which is what makes a conflict converge
    /// in the round that finds it instead of being re-decided, silently and the
    /// other way, by the next.
    #[test]
    fn an_organizer_edit_lands_and_the_newer_revision_wins_a_double_edit() {
        let mut base = snap();
        base.notes.push(note(1, "shared"));
        base.tasks.push(task(3, 0, "shared"));
        let shadow = base.clone();

        // The phone moved both rows; this device did not.
        let mut remote = base.clone();
        remote.notes = vec![note_at(remote.notes[0].clone(), 1_700_001_000_000, "phone", |n| {
            n.title = "renamed there".into()
        })];
        remote.tasks = vec![task_at(remote.tasks[0].clone(), 1_700_001_000_000, "phone", |t| {
            t.done = true
        })];

        let out = with_ctx(0, |ctx| merge(&base, Some(&shadow), &remote, "phone", ctx));
        assert_eq!(out.merged.notes[0].title, "renamed there");
        assert!(out.merged.tasks[0].done);
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);

        // Both moved and the phone's write is the later one: it stands, and the
        // round says so — which is the only thing that can explain the edit this
        // device made and no longer sees.
        let mut local = base.clone();
        local.notes = vec![note_at(local.notes[0].clone(), 1_700_000_500_000, "desk", |n| {
            n.title = "renamed here".into()
        })];
        let out = with_ctx(0, |ctx| merge(&local, Some(&shadow), &remote, "phone", ctx));
        assert_eq!(out.merged.notes[0].title, "renamed there", "the newer write stands");
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
        assert!(out.conflicts[0].contains("note"), "{:?}", out.conflicts);

        // And with the clock the other way this device's write is the one that
        // stands: the arbiter is the revision, not which end is asking.
        let mut local = base.clone();
        local.notes = vec![note_at(local.notes[0].clone(), 1_700_002_000_000, "desk", |n| {
            n.title = "renamed here".into()
        })];
        let out = with_ctx(0, |ctx| merge(&local, Some(&shadow), &remote, "phone", ctx));
        assert_eq!(out.merged.notes[0].title, "renamed here", "the newer write stands");
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);

        // A **purge** is the one removal with no row to carry it, so it is the one
        // thing the shadow still decides: an untouched local row the peer no longer
        // has stays gone.
        let out = with_ctx(0, |ctx| merge(&base, Some(&shadow), &snap(), "phone", ctx));
        assert!(out.merged.notes.is_empty());
        assert!(out.merged.tasks.is_empty());
    }

    /// 回收站 (SPEC §四十一): a bin is a write of the row like any other — which is
    /// exactly why the revision is a field of its own, since `edited` deliberately
    /// does not move when a row is binned. So the bin travels, the arbiter orders it
    /// against an edit made elsewhere, and a row binned on both sides is two peers
    /// agreeing rather than a disagreement.
    #[test]
    fn a_bin_travels_as_a_write_of_the_row_it_belongs_to() {
        let mut base = snap();
        base.notes.push(note(1, "shared"));
        let shadow = base.clone();

        // Binned on the phone, untouched here: the bin lands.
        let mut binned = base.clone();
        binned.notes = vec![note_at(binned.notes[0].clone(), 1_700_001_000_000, "phone", |n| {
            n.deleted_at = Some(1_700_001_000)
        })];
        let out = with_ctx(0, |ctx| merge(&base, Some(&shadow), &binned, "phone", ctx));
        assert_eq!(
            out.merged.notes[0].deleted_at,
            Some(1_700_001_000),
            "the remote bin landed"
        );
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);

        // Binned on both sides: the same row twice is agreement, not a conflict.
        let out = with_ctx(0, |ctx| merge(&binned, Some(&shadow), &binned, "phone", ctx));
        assert_eq!(out.merged.notes[0].deleted_at, Some(1_700_001_000));
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);

        // Binned here, edited there, the edit the later write: the edit stands —
        // including the un-binning, because the row the later write describes is a
        // live one. The round says so.
        let mut edited = base.clone();
        edited.notes = vec![note_at(edited.notes[0].clone(), 1_700_002_000_000, "phone", |n| {
            n.title = "renamed there".into()
        })];
        let out = with_ctx(0, |ctx| merge(&binned, Some(&shadow), &edited, "phone", ctx));
        assert_eq!(out.merged.notes[0].deleted_at, None, "the later write un-binned it");
        assert_eq!(out.merged.notes[0].title, "renamed there");
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
        assert!(out.conflicts[0].contains("note"), "{:?}", out.conflicts);

        // …and the other way round the bin is the later write, so it stands.
        let mut binned_later = base.clone();
        binned_later.notes = vec![note_at(
            binned_later.notes[0].clone(),
            1_700_003_000_000,
            "desk",
            |n| n.deleted_at = Some(1_700_003_000),
        )];
        let out = with_ctx(0, |ctx| merge(&binned_later, Some(&shadow), &edited, "phone", ctx));
        assert_eq!(out.merged.notes[0].deleted_at, Some(1_700_003_000), "the bin stands");
        assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    }

    /// The one pointer in the area. Two devices that each minted list 5 keep
    /// both lists, and every task that arrived pointing at the remote one is
    /// remapped onto the id it actually landed under — while the local list's
    /// tasks stay where they were and the inbox stays the inbox.
    #[test]
    fn a_colliding_list_id_renumbers_and_the_tasks_that_named_it_follow() {
        let mut local = snap();
        local.lists.push(list(5, "local list"));
        local.tasks.push(task(20, 5, "points at the local list"));

        let mut remote = snap();
        remote.lists.push(list(5, "remote list"));
        remote.tasks.push(task(21, 5, "points at the remote list"));
        remote.tasks.push(task(22, 0, "in the inbox"));

        let out = with_ctx(1000, |ctx| merge(&local, Some(&snap()), &remote, "phone", ctx));
        assert!(
            out.merged.lists.iter().any(|l| l.id == 5 && l.name == "local list"),
            "the local list keeps its id"
        );
        let twin = out
            .merged
            .lists
            .iter()
            .find(|l| l.id != 5)
            .expect("the colliding remote list was renumbered rather than lost");
        assert_eq!(twin.name, "remote list");

        let mine = out.merged.tasks.iter().find(|t| t.id == 20).unwrap();
        assert_eq!(mine.list, 5, "a local task's list is this device's and did not move");
        let theirs = out
            .merged
            .tasks
            .iter()
            .find(|t| t.title == "points at the remote list")
            .unwrap();
        assert_eq!(theirs.list, twin.id, "it followed the list it arrived with");
        let inbox = out
            .merged
            .tasks
            .iter()
            .find(|t| t.title == "in the inbox")
            .unwrap();
        assert_eq!(inbox.list, 0, "the sentinel is not a list and cannot be remapped");
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
    }

    /// The organizer's one *self*-reference: a note pointing at a note. A comment
    /// that arrived from the peer carries a `ref_note` naming the **remote** integer,
    /// so it has to be walked through the `remote id → local id` map the uuid-keyed
    /// pass built — or it would end up answering whatever this device happens to
    /// hold under that integer, which here is an unrelated local note.
    #[test]
    fn a_comment_follows_its_parent_to_the_integer_it_landed_under() {
        // A local note already sits on the integer the remote parent also uses, so
        // the parent cannot keep it and must land under a fresh one.
        let mut local = snap();
        local.notes.push(note(5, "local note"));

        let mut remote = snap();
        remote.notes.push(note(5, "remote parent"));
        remote.notes[0].uuid = "d".repeat(32);
        remote.notes.push(comment(6, "the reply", 5));

        let out = with_ctx(1000, |ctx| merge(&local, Some(&snap()), &remote, "phone", ctx));

        let parent = out
            .merged
            .notes
            .iter()
            .find(|n| n.title == "remote parent")
            .expect("the remote parent arrived");
        assert_ne!(parent.id, 5, "it could not take the local note's integer");
        let reply = out
            .merged
            .notes
            .iter()
            .find(|n| n.title == "the reply")
            .expect("the reply survived the merge");
        assert_eq!(
            reply.ref_note,
            Some(parent.id),
            "the reply follows the parent it answers"
        );
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
    }

    /// A reply whose parent is nowhere: the ref rides through **unchanged** rather
    /// than being cleared. The comment stays a comment, and if the parent arrives
    /// in a later sync the ref already points at it — the `page_ref` rule.
    #[test]
    fn a_ref_to_a_note_that_did_not_come_leaves_the_comment_a_comment() {
        let mut remote = snap();
        remote.notes.push(comment(9, "orphan reply", 404));

        let out = with_ctx(1000, |ctx| merge(&snap(), None, &remote, "phone", ctx));

        let reply = out
            .merged
            .notes
            .iter()
            .find(|n| n.title == "orphan reply")
            .expect("the reply arrived");
        assert_eq!(reply.ref_note, Some(404), "a dangling ref is kept, not scrubbed");
    }
}
